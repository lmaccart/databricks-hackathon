/* Entry point of the shipped image (ours, not Team 67's).
 *
 * Team 67's pipeline (cpp/pipeline, MIT) is fast on the layout the generator
 * writes, but rules.md lists the CSV columns without promising their order: with
 * a reordered events header that binary exits 0 with wrong totals. This launcher
 * reads only header lines and the manifest's shape. When every input is in the
 * canonical layout it execs the fast pipeline; anything else (other column
 * order, quoted or \r-terminated headers, non-ISO manifest values, missing or
 * unreadable folders) goes to the general Rust pipeline, which maps columns by
 * name. The choice never depends on row contents, sizes or IDs.
 *
 * The fast pipeline also needs AVX-512 VBMI2 to be fast: on an AMD EPYC 7763
 * (no AVX-512) it took 70-89% longer than the Rust pipeline on warm 20M and 60M
 * inputs in paired x86 Docker runs (CI runs 44958622548 and 44959388893), while
 * its authors measured it about 3.5x faster on a Xeon 6975P-C. So it runs only
 * on x86-64 CPUs that report AVX-512 BW and VBMI2 with OS-enabled ZMM state.
 *
 * SS_FAST_BIN and SS_GENERAL_BIN override the two executables for native tests;
 * SS_FORCE_FAST=1 skips the CPU check (tests on other hosts).
 */
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#if defined(__x86_64__)
#include <cpuid.h>
#endif

/* AVX-512 BW and VBMI2, and the OS saves opmask/ZMM state (XCR0 bits 1,2,5,6,7). */
static int cpu_fast(void) {
#if defined(__x86_64__)
    unsigned a, b, c, d;
    if (!__get_cpuid(1, &a, &b, &c, &d) || !(c & (1u << 27))) return 0; /* OSXSAVE */
    unsigned lo, hi;
    __asm__ volatile("xgetbv" : "=a"(lo), "=d"(hi) : "c"(0));
    if ((lo & 0xe6) != 0xe6) return 0;
    if (!__get_cpuid_count(7, 0, &a, &b, &c, &d)) return 0;
    return (b & (1u << 30)) && (c & (1u << 6)); /* AVX512BW, AVX512_VBMI2 */
#else
    return 0;
#endif
}

static const char *data_dir;

/* True iff path's first line is exactly `want` followed by '\n' (or EOF). */
static int header_is(const char *path, const char *want) {
    char buf[256];
    size_t n = strlen(want);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t got = read(fd, buf, n + 1);
    close(fd);
    if (got < (ssize_t)n || memcmp(buf, want, n) != 0) return 0;
    return got == (ssize_t)n || buf[n] == '\n';
}

static int ends_with_csv(const char *name) {
    size_t n = strlen(name);
    return n >= 4 && strcmp(name + n - 4, ".csv") == 0;
}

/* Every *.csv in DATA_DIR/sub has the canonical header. */
static int folder_ok(const char *sub, const char *want) {
    char path[4096];
    snprintf(path, sizeof path, "%s/%s", data_dir, sub);
    DIR *d = opendir(path);
    if (!d) return 0;
    int ok = 1;
    for (struct dirent *e; ok && (e = readdir(d));) {
        if (!ends_with_csv(e->d_name)) continue;
        char file[8192];
        snprintf(file, sizeof file, "%s/%s", path, e->d_name);
        ok = header_is(file, want);
    }
    closedir(d);
    return ok;
}

static int file_ok(const char *name, const char *want) {
    char path[4096];
    snprintf(path, sizeof path, "%s/%s", data_dir, name);
    return header_is(path, want);
}

/* `"key"` is followed by `:`, optional spaces, and a quoted F1 timestamp
 * YYYY-MM-DDTHH:MM:SSZ (digit positions checked; calendar left to the pipeline). */
static int manifest_value_ok(const char *json, const char *key) {
    const char *p = strstr(json, key);
    if (!p) return 0;
    p += strlen(key);
    while (*p == ' ') p++;
    if (*p++ != ':') return 0;
    while (*p == ' ') p++;
    if (*p++ != '"') return 0;
    static const char shape[] = "dddd-dd-ddTdd:dd:ddZ";
    for (int i = 0; shape[i]; i++, p++) {
        if (shape[i] == 'd' ? (*p < '0' || *p > '9') : *p != shape[i]) return 0;
    }
    return *p == '"';
}

static int manifest_ok(void) {
    char path[4096], buf[4096];
    snprintf(path, sizeof path, "%s/manifest.json", data_dir);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0 || n == (ssize_t)sizeof buf - 1) return 0;
    buf[n] = 0;
    return manifest_value_ok(buf, "\"window_start\"") && manifest_value_ok(buf, "\"window_end\"");
}

/* Input pages that are not resident make the pipeline's first pass wait on
 * storage one fault at a time. PREFETCH_WORKERS short-lived children ask the
 * kernel to read every input CSV ahead (POSIX_FADV_WILLNEED), sharing the files
 * round-robin, while this process checks headers and execs the pipeline. Advice
 * never changes file contents, so output is unaffected; on resident pages it
 * is a cheap no-op. SS_PREFETCH=0 disables it. */
#define PREFETCH_WORKERS 16
static void prefetch_inputs(void) {
#if defined(__linux__)
    const char *env = getenv("SS_PREFETCH");
    if (env && strcmp(env, "0") == 0) return;
    static const char *subs[] = {"links", "events", "clicks"};
    for (int k = 0; k < PREFETCH_WORKERS; k++) {
        pid_t pid = fork();
        if (pid != 0) continue; /* parent (or fork failure): carry on */
        int i = 0;
        for (unsigned s = 0; s < sizeof subs / sizeof *subs; s++) {
            char path[4096];
            snprintf(path, sizeof path, "%s/%s", data_dir, subs[s]);
            DIR *d = opendir(path);
            if (!d) continue;
            for (struct dirent *e; (e = readdir(d));) {
                if (!ends_with_csv(e->d_name) || i++ % PREFETCH_WORKERS != k) continue;
                char file[8192];
                snprintf(file, sizeof file, "%s/%s", path, e->d_name);
                int fd = open(file, O_RDONLY);
                if (fd >= 0) {
                    posix_fadvise(fd, 0, 0, POSIX_FADV_WILLNEED);
                    close(fd);
                }
            }
            closedir(d);
        }
        _exit(0);
    }
#endif
}

int main(int argc, char **argv) {
    (void)argc;
    data_dir = getenv("DATA_DIR");
    if (!data_dir || !*data_dir) data_dir = "/data";
    prefetch_inputs();
    const char *fast = getenv("SS_FAST_BIN");
    const char *general = getenv("SS_GENERAL_BIN");
    if (!fast) fast = "/app/shopping";
    if (!general) general = "/app/shopping-general";
    const char *force = getenv("SS_FORCE_FAST");
    int cpu = (force && strcmp(force, "1") == 0) || cpu_fast();
    int canonical = manifest_ok()
        && folder_ok("events", "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents")
        && folder_ok("clicks", "click_id,user_id,campaign_id,ts")
        && folder_ok("links", "user_id_a,user_id_b")
        && file_ok("products.csv", "product_id,category")
        && file_ok("campaigns.csv", "campaign_id,channel");
    const char *target = canonical && cpu ? fast : general;
    if (!cpu) fprintf(stderr, "[launch] no AVX-512 VBMI2: general pipeline\n");
    else if (!canonical) fprintf(stderr, "[launch] non-canonical input layout: general pipeline\n");
    argv[0] = (char *)target;
    execv(target, argv);
    perror("[launch] exec");
    return 127;
}
