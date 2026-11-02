/* The one place this server calls the Phonon-2 engine (vendor/phonon-2-c).
 *
 * A shim rather than Rust declarations of the engine's structs: P2Result is a
 * dozen fields of pointers, ints and doubles, and mirroring it by hand in Rust
 * is a layout that can drift silently when the vendored copy is updated. Here
 * the compiler checks it, and Rust sees three functions and two opaque
 * pointers.
 *
 * The thread pool is the caller's. p2_engine_open and p2_transcribe both run on
 * whatever p2_pool() returns on the calling thread, which is a process-wide
 * singleton sized from P2_THREADS unless the thread has set its own. Setting
 * an environment variable from a multi-threaded Rust process is a data race, so
 * the server creates a pool of the size it wants and installs it around each
 * call instead, and clears it again so a later call on the same (tokio
 * blocking-pool) thread never sees a pool that has since been destroyed. */
#include <stdlib.h>
#include <string.h>

#include "phonon2.h"
#include "threadpool.h"

void *hearsay_p2_pool_create(int n_threads) { return p2_pool_create(n_threads); }

void hearsay_p2_pool_destroy(void *pool) { p2_pool_destroy((ThreadPool *)pool); }

/* p2_verbose prints a progress line per encoder block, meant for a terminal; in
 * a server it is 24 carriage-return-separated lines per memo in the container
 * log. The server logs its own one line per transcription instead. Written
 * before the engine exists, so before anything can read it. */
void *hearsay_p2_open(const char *weights_dir, void *pool)
{
    p2_verbose = 0;
    p2_set_pool((ThreadPool *)pool);
    P2Engine *e = p2_engine_open(weights_dir);
    p2_set_pool(NULL);
    return e;
}

void hearsay_p2_close(void *engine) { p2_engine_close((P2Engine *)engine); }

/* Mono float32 at `rate` → a malloc'd UTF-8 transcript in *text (free it with
 * hearsay_p2_free), plus the stage timings. Returns 0, or -1 for an input the
 * engine refused or memory it could not get. */
int hearsay_p2_transcribe(void *engine, void *pool, const float *audio, int n_samples, int rate,
                          char **text, double *seconds_audio, double *seconds_encoder)
{
    *text = NULL;
    *seconds_audio = 0.0;
    *seconds_encoder = 0.0;
    P2Result r;
    memset(&r, 0, sizeof(r));
    p2_set_pool((ThreadPool *)pool);
    int rc = p2_transcribe((const P2Engine *)engine, audio, n_samples, rate, &r);
    p2_set_pool(NULL);
    if (rc != 0) {
        p2_result_free(&r);
        return -1;
    }
    const char *t = r.text ? r.text : "";
    size_t n = strlen(t);
    char *out = (char *)malloc(n + 1);
    if (out) memcpy(out, t, n + 1);
    *seconds_audio = r.duration_seconds;
    *seconds_encoder = r.t_encoder;
    p2_result_free(&r);
    if (!out) return -1;
    *text = out;
    return 0;
}

void hearsay_p2_free(char *text) { free(text); }
