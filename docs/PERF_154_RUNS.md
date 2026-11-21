# Native Linux benchmark runs: 1.54 s round

These are hosted CI comparisons, not organizer times. Each row comes from a completed job with timings on **both** 20M seeds. Each arrow is control → candidate, in seconds; parentheses are candidate wins / pairs. The last two columns are full `docker run`; a dash means that scope was not run. CPU models vary, so compare across an arrow, not down a column.

**65 two-seed jobs**, **45 distinct candidate revisions** including diagnostic controls. [Interpretation and practice pick](PERF_154.md).

| CI run | Candidate / control | CPU | 20M seed 7 native | 20M seed 15 native | 20M Docker | 60M Docker |
|---|---|---|---:|---:|---:|---:|
| [scorer-clock](https://github.com/lmaccart/databricks-hackathon/actions/runs/44933674959) | `c090efb` / `7fd1b76` | Intel Xeon 6975P-C | 1.682 → 1.687 (3/5) | 1.662 → 1.659 (3/5) | 1.781 → 1.783 (2/5) | — |
| [stripe](https://github.com/lmaccart/databricks-hackathon/actions/runs/44933675050) | `e413b41` / `7fd1b76` | AMD EPYC 7763 | 2.104 → 2.214 (0/5) | 2.090 → 2.205 (0/5) | — | — |
| [narrow](https://github.com/lmaccart/databricks-hackathon/actions/runs/44933922444) | `a32d0b7` / `7fd1b76` | Intel Xeon 6975P-C | 1.683 → 1.636 (4/5) | 1.653 → 1.612 (5/5) | — | — |
| [radix-rewrite](https://github.com/lmaccart/databricks-hackathon/actions/runs/44933986127) | `e44a7b9` / `7fd1b76` | Intel Xeon 6975P-C | 1.677 → 1.752 (0/5) | 1.650 → 1.797 (0/5) | — | — |
| [assist-ranges](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934066598) | `3e25a24` / `7fd1b76` | AMD EPYC 7763 | 2.130 → 2.118 (4/5) | 2.088 → 2.045 (5/5) | — | — |
| [lazy-clock](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934066640) | `1c95cad` / `7fd1b76` | Intel Xeon 6975P-C | 1.681 → 1.684 (2/5) | 1.672 → 1.667 (2/5) | — | — |
| [pgo](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934102542) | `5034e0c` / `7fd1b76` | Intel Xeon 6975P-C | 1.730 → 1.833 (0/5) | 1.668 → 1.778 (0/5) | 1.789 → 1.897 (0/5) | 5.463 → 5.725 (0/3) |
| [motif](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934274229) | `09f159f` / `7fd1b76` | Intel XEON PLATINUM 8573C | 2.168 → 2.197 (0/5) | 2.115 → 2.178 (0/5) | — | — |
| [colout](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934274560) | `0c5a18e` / `7fd1b76` | Intel Xeon 6975P-C | 1.673 → 1.630 (5/5) | 1.659 → 1.597 (5/5) | — | — |
| [nt-claims](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934277810) | `64ee9bb` / `7fd1b76` | Intel Xeon 6975P-C | 1.721 → 1.728 (2/5) | 1.701 → 1.706 (2/5) | — | — |
| [mixed-radix](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934323811) | `2915f08` / `7fd1b76` | AMD EPYC 9V45 | 1.460 → 1.546 (0/5) | 1.427 → 1.500 (0/5) | — | — |
| [slotkeys](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934368753) | `ba5a91d` / `7fd1b76` | Intel Xeon 6975P-C | 1.663 → 1.669 (2/5) | 1.620 → 1.647 (0/5) | — | — |
| [highway-pack](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934661503) | `d38e1d5` / `7fd1b76` | AMD EPYC 9V45 | 1.659 → 1.559 (5/5) | 1.615 → 1.622 (3/5) | 2.026 → 1.975 (5/5) | 6.208 → 6.274 (1/3) |
| [rowmerge](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934668743) | `4ad0e5d` / `7fd1b76` | Intel Xeon 6975P-C | 1.692 → 1.590 (5/5) | 1.633 → 1.555 (5/5) | 1.816 → 1.722 (5/5) | 5.532 → 5.199 (3/3) |
| [ssse3-clock](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935024259) | `78b0ba1` / `7fd1b76` | AMD EPYC 7763 | 2.084 → 2.097 (0/5) | 2.059 → 2.061 (4/5) | — | — |
| [doublerank](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935176864) | `3aa732f` / `7fd1b76` | Intel Xeon 6975P-C | 1.700 → 1.589 (5/5) | 1.644 → 1.549 (5/5) | — | — |
| [combined-vs-columns](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935189403) | `4ad0e5d` / `0c5a18e` | Intel Xeon Platinum 8370C | 2.104 → 2.079 (5/5) | 2.254 → 2.184 (5/5) | 2.360 → 2.361 (2/5) | 6.843 → 6.850 (1/3) |
| [combined-vs-ranks](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935189493) | `4ad0e5d` / `a32d0b7` | Intel Xeon Platinum 8370C | 2.347 → 2.257 (5/5) | 2.260 → 2.145 (5/5) | 2.482 → 2.372 (5/5) | 7.736 → 7.438 (3/3) |
| [directranges](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935266183) | `be4a89c` / `a32d0b7` | Intel Xeon 6975P-C | 1.651 → 1.594 (5/5) | 1.631 → 1.591 (5/5) | — | — |
| [click-runs](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935456168) | `48de109` / `a32d0b7` | Intel Xeon 6975P-C | 1.667 → 1.660 (3/5) | 1.665 → 1.658 (4/5) | — | — |
| [peel](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935456488) | `14ae526` / `7fd1b76` | Intel Xeon 6975P-C | 1.692 → 1.768 (0/5) | 1.650 → 1.711 (0/5) | — | — |
| [graph-slots-pack](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935468441) | `3aa732f` / `7fd1b76` | AMD EPYC 9V74 | 1.744 → 1.703 (4/5) | 1.725 → 1.641 (5/5) | 2.227 → 2.150 (4/5) | 6.723 → 6.257 (3/3) |
| [slots-columns](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935468450) | `4933dbe` / `7fd1b76` | AMD EPYC 7763 | 2.088 → 1.985 (5/5) | 2.066 → 1.939 (5/5) | 2.258 → 2.096 (5/5) | 6.870 → 6.332 (3/3) |
| [faster-graph](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935485266) | `4933dbe` / `4ad0e5d` | Intel Xeon 6975P-C | 1.606 → 1.556 (5/5) | 1.593 → 1.541 (5/5) | 1.729 → 1.681 (5/5) | 5.170 → 4.994 (3/3) |
| [newer-columns](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935485357) | `4933dbe` / `3aa732f` | Intel Xeon 6975P-C | 1.595 → 1.530 (5/5) | 1.579 → 1.508 (5/5) | 1.697 → 1.642 (5/5) | 5.335 → 5.007 (3/3) |
| [lane-graphs](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935516680) | `ddd4fe1` / `7fd1b76` | Intel Xeon 6975P-C | 1.685 → 1.796 (0/5) | 1.626 → 1.724 (0/5) | — | — |
| [reverse-lf](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935522155) | `f558a79` / `7fd1b76` | Intel Xeon 6975P-C | 1.709 → 1.818 (0/5) | 1.683 → 1.784 (0/5) | — | — |
| [twoprefix](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935548399) | `89e5b49` / `7fd1b76` | Intel Xeon 6975P-C | 1.650 → 1.659 (1/5) | 1.630 → 1.623 (4/5) | — | — |
| [populate](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935576835) | `b083592` / `7fd1b76` | AMD EPYC 7763 | 2.078 → 2.131 (1/5) | 2.074 → 2.127 (1/5) | — | — |
| [pooled-pages](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935576863) | `2c067f2` / `4ad0e5d` | Intel Xeon 6975P-C | 1.552 → 1.540 (4/5) | 1.559 → 1.562 (1/5) | — | — |
| [gapscan](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935698391) | `b05f811` / `4933dbe` | Intel Xeon 6975P-C | 1.531 → 1.547 (2/5) | 1.486 → 1.495 (2/5) | — | — |
| [ranges-vs-scored](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935810599) | `0558eaa` / `7fd1b76` | AMD EPYC 9V45 | 1.360 → 1.225 (5/5) | 1.341 → 1.186 (5/5) | 1.792 → 1.617 (5/5) | 5.451 → 4.618 (3/3) |
| [slotranges](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935810612) | `0558eaa` / `4933dbe` | AMD EPYC 9V74 | 1.627 → 1.552 (5/5) | 1.582 → 1.524 (5/5) | 2.042 → 1.968 (5/5) | 5.917 → 5.734 (3/3) |
| [count-pages-fix](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935845158) | `c7cf212` / `4ad0e5d` | Intel Xeon 6975P-C | 1.579 → 1.576 (4/5) | 1.555 → 1.532 (4/5) | — | — |
| [live-meter](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935956505) | `4922304` / `4933dbe` | AMD EPYC 7763 | 1.967 → 1.866 (5/5) | 1.935 → 1.861 (4/5) | — | — |
| [worker-grid](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936005555) | `edd5c36` / `4ad0e5d` | AMD EPYC 9V74 | 1.700 → 1.591 (5/5) | 1.711 → 1.576 (5/5) | — | — |
| [portfolio-verify](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936040920) | `4933dbe` / `7fd1b76` | Intel Xeon 6975P-C | 1.652 → 1.494 (5/5) | 1.657 → 1.496 (5/5) | 1.816 → 1.651 (5/5) | 5.336 → 4.852 (3/3) |
| [splitclock](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936134526) | `994b664` / `4933dbe` | Intel Xeon 6975P-C | 1.531 → 1.547 (1/5) | 1.498 → 1.493 (3/5) | — | — |
| [typed-floor](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936182346) | `afa3af4` / `4933dbe` | Intel Xeon 6975P-C | 1.527 → 1.537 (2/5) | 1.522 → 1.513 (4/5) | — | — |
| [meter-pack](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936299795) | `4922304` / `4933dbe` | Intel Xeon 6975P-C | 1.494 → 1.463 (5/5) | 1.469 → 1.443 (5/5) | 1.585 → 1.563 (5/5) | 4.719 → 4.663 (2/3) |
| [grid-pack](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936299807) | `edd5c36` / `4ad0e5d` | Intel Xeon Platinum 8370C | 2.125 → 1.936 (5/5) | 2.103 → 1.928 (5/5) | 2.281 → 2.090 (5/5) | 6.923 → 6.480 (3/3) |
| [meter-ranges](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936375552) | `f669c11` / `0558eaa` | Intel Xeon 6975P-C | 1.496 → 1.476 (4/5) | 1.454 → 1.434 (4/5) | 1.602 → 1.593 (3/5) | 4.779 → 4.719 (2/3) |
| [stripe-claims](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936377632) | `1e1de5d` / `4933dbe` | Intel Xeon 6975P-C | 1.500 → 1.580 (0/5) | 1.492 → 1.560 (0/5) | — | — |
| [mimalloc3](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936526432) | `29ccc90` / `0558eaa` | Intel Xeon 6975P-C | 1.463 → 1.267 (5/5) | 1.462 → 1.280 (5/5) | 1.614 → 1.404 (5/5) | 4.801 → 4.174 (3/3) |
| [hot-scatter](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936599146) | `a580c30` / `0558eaa` | Intel Xeon 6975P-C | 1.484 → 1.489 (1/5) | 1.449 → 1.452 (1/5) | — | — |
| [gridrange-vs-scored](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936619511) | `d754296` / `7fd1b76` | Intel Xeon 6975P-C | 1.679 → 1.372 (5/5) | 1.639 → 1.340 (5/5) | 1.779 → 1.471 (5/5) | 5.512 → 4.448 (3/3) |
| [gridrange](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936619619) | `d754296` / `0558eaa` | Intel Xeon 6975P-C | 1.481 → 1.364 (5/5) | 1.419 → 1.296 (5/5) | 1.573 → 1.455 (5/5) | 4.731 → 4.474 (3/3) |
| [fullfold](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936619670) | `7619fa6` / `7fd1b76` | AMD EPYC 7763 | 2.057 → 1.659 (5/5) | 2.037 → 1.625 (5/5) | 2.239 → 1.837 (5/5) | 7.079 → 5.434 (3/3) |
| [grid-on-allocation](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936631125) | `7619fa6` / `f669c11` | Intel Xeon Platinum 8370C | 1.976 → 1.859 (5/5) | 1.953 → 1.810 (5/5) | 2.072 → 1.928 (5/5) | 6.378 → 6.181 (3/3) |
| [allocation-on-grid](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936631348) | `7619fa6` / `d754296` | Intel Xeon 6973P-C | 1.564 → 1.529 (4/5) | 1.527 → 1.510 (2/5) | 2.044 → 1.990 (4/5) | 5.968 → 5.932 (3/3) |
| [compactdir](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936657547) | `7b9d1c9` / `0558eaa` | Intel XEON PLATINUM 8573C | 1.896 → 1.891 (1/5) | 1.848 → 1.830 (3/5) | — | — |
| [range-on-grid](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936740314) | `7619fa6` / `ff3cc6b` | Intel Xeon 6975P-C | 1.386 → 1.372 (5/5) | 1.401 → 1.362 (5/5) | 1.517 → 1.488 (5/5) | 4.642 → 4.598 (3/3) |
| [mi-grid-verify](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937040474) | `14ccc10` / `7fd1b76` | AMD EPYC 9V74 | 1.744 → 1.113 (5/5) | 1.731 → 1.116 (5/5) | 2.359 → 1.683 (4/5) | 6.617 → 4.627 (3/3) |
| [folds-vs-allocator](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937040599) | `14ccc10` / `29ccc90` | AMD EPYC 7763 | 1.425 → 1.321 (5/5) | 1.402 → 1.297 (5/5) | 1.577 → 1.482 (5/5) | 4.827 → 4.482 (3/3) |
| [final-mi](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937040811) | `894f6a7` / `7fd1b76` | Intel Xeon 6975P-C | 1.670 → 1.161 (5/5) | 1.654 → 1.149 (5/5) | 1.793 → 1.267 (5/5) | 5.437 → 3.851 (3/3) |
| [allocator-on-grid](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937041255) | `894f6a7` / `7619fa6` | Intel Xeon 6975P-C | 1.327 → 1.150 (5/5) | 1.293 → 1.142 (5/5) | 1.434 → 1.254 (5/5) | 4.490 → 3.868 (3/3) |
| [allocator-reserves](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937042928) | `894f6a7` / `14ccc10` | AMD EPYC 7763 | 1.329 → 1.310 (5/5) | 1.311 → 1.303 (2/5) | 1.487 → 1.481 (3/5) | 4.619 → 4.523 (3/3) |
| [keep-pages](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937148686) | `f73e16d` / `14ccc10` | Intel Xeon 6975P-C | 1.130 → 1.120 (4/5) | 1.133 → 1.113 (5/5) | 1.245 → 1.231 (5/5) | 3.811 → 3.663 (3/3) |
| [huge-aligned](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937148704) | `6a18c94` / `14ccc10` | Intel Xeon 6975P-C | 1.180 → 1.171 (4/5) | 1.153 → 1.145 (3/5) | 1.296 → 1.277 (4/5) | 3.886 → 3.815 (3/3) |
| [final-memory](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937264092) | `14ccc10` / `7fd1b76` | AMD EPYC 7763 | 2.100 → 1.349 (5/5) | 2.081 → 1.330 (5/5) | 2.357 → 1.552 (5/5) | 7.120 → 4.729 (3/3) |
| [second60-gridmi](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937344723) | `14ccc10` / `7fd1b76` | AMD EPYC 9V74 | 1.727 → 1.119 (5/5) | 1.720 → 1.116 (5/5) | 2.239 → 1.541 (5/5) | 6.633 → 4.676 (3/3) |
| [second60-mi](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937347254) | `894f6a7` / `7fd1b76` | AMD EPYC 7763 | 2.073 → 1.311 (5/5) | 2.061 → 1.308 (5/5) | 2.248 → 1.467 (5/5) | 6.840 → 4.473 (3/3) |
| [selected-profile](https://github.com/lmaccart/databricks-hackathon/actions/runs/44938196873) | `1a3d0fd` / `14ccc10` | Intel Xeon 6975P-C | 1.161 → 1.170 (1/5) | 1.139 → 1.150 (3/5) | — | — |
| [main-release](https://github.com/lmaccart/databricks-hackathon/actions/runs/44938836557) | `1a3d0fd` / `7fd1b76` | Intel Xeon 6975P-C | 1.682 → 1.165 (5/5) | 1.672 → 1.172 (5/5) | 1.825 → 1.294 (5/5) | 5.508 → 3.871 (3/3) |
| [compact-ranked-clicks](https://github.com/lmaccart/databricks-hackathon/actions/runs/44938958482) | `d4f2df2` / `14ccc10` | Intel Xeon 6975P-C | 1.131 → 1.142 (0/5) | 1.121 → 1.137 (2/5) | 1.231 → 1.235 (1/5) | 3.758 → 3.805 (1/3) |
