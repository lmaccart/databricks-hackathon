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
 * SS_FAST_BIN and SS_GENERAL_BIN override the two executables for native tests.
 */
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

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

int main(int argc, char **argv) {
    (void)argc;
    data_dir = getenv("DATA_DIR");
    if (!data_dir || !*data_dir) data_dir = "/data";
    const char *fast = getenv("SS_FAST_BIN");
    const char *general = getenv("SS_GENERAL_BIN");
    if (!fast) fast = "/app/shopping";
    if (!general) general = "/app/shopping-general";
    int canonical = manifest_ok()
        && folder_ok("events", "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents")
        && folder_ok("clicks", "click_id,user_id,campaign_id,ts")
        && folder_ok("links", "user_id_a,user_id_b")
        && file_ok("products.csv", "product_id,category")
        && file_ok("campaigns.csv", "campaign_id,channel");
    const char *target = canonical ? fast : general;
    if (!canonical) fprintf(stderr, "[launch] non-canonical input layout: general pipeline\n");
    argv[0] = (char *)target;
    execv(target, argv);
    perror("[launch] exec");
    return 127;
}
