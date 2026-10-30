/* Link to the exact Cargo bundled archive. Synthetic data only. */
#define _POSIX_C_SOURCE 200809L
#include "sqlite3.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
static sqlite3_mem_methods original;
static int counting;
static long long allocations, bytes;
static void *allocate(int n) {
  void *p = original.xMalloc(n);
  if (p && counting) {
    allocations++;
    bytes += n;
  }
  return p;
}
static void *reallocate(void *p, int n) {
  void *out = original.xRealloc(p, n);
  if (out && counting) {
    allocations++;
    bytes += n;
  }
  return out;
}
static void ok(int rc) {
  if (rc != SQLITE_OK) {
    fprintf(stderr, "SQLite %d\n", rc);
    exit(1);
  }
}
static long long ns(clockid_t id) {
  struct timespec t;
  if (clock_gettime(id, &t))
    abort();
  return (long long)t.tv_sec * 1000000000 + t.tv_nsec;
}
static void bind(sqlite3_stmt *s, int bound, int absent) {
  ok(sqlite3_bind_int(s, 1, 1));
  ok(sqlite3_bind_text(s, 2, absent ? "missing" : "synthetic", -1,
                       SQLITE_STATIC));
  if (bound)
    ok(sqlite3_bind_int64(s, 3, 1));
}
int main(void) {
  ok(sqlite3_config(SQLITE_CONFIG_GETMALLOC, &original));
  sqlite3_mem_methods measured = original;
  measured.xMalloc = allocate;
  measured.xRealloc = reallocate;
  ok(sqlite3_config(SQLITE_CONFIG_MALLOC, &measured));
  sqlite3 *db = 0;
  ok(sqlite3_open(":memory:", &db));
  ok(sqlite3_exec(db,
                  "CREATE TABLE contacts(device_id INTEGER NOT NULL,jid TEXT "
                  "NOT NULL,push_name TEXT,full_name TEXT,first_name "
                  "TEXT,business_name TEXT,PRIMARY KEY(device_id,jid));"
                  "CREATE TABLE lid_pn_mapping(device_id INTEGER NOT NULL,lid "
                  "TEXT NOT NULL,phone_number TEXT NOT NULL,created_at BIGINT "
                  "NOT NULL,updated_at BIGINT NOT NULL,learning_source TEXT "
                  "NOT NULL,PRIMARY KEY(lid,device_id));"
                  "CREATE INDEX idx_lid_pn_mapping_phone ON "
                  "lid_pn_mapping(phone_number,device_id);"
                  "INSERT INTO contacts "
                  "VALUES(1,'synthetic',NULL,'name',NULL,NULL),(2,'synthetic',"
                  "NULL,'other account',NULL,NULL);"
                  "INSERT INTO lid_pn_mapping "
                  "VALUES(1,'synthetic','synthetic',1,2,'test'),(1,'older','"
                  "synthetic',1,1,'test'),(1,'tie','synthetic',1,2,'test'),(2,'"
                  "synthetic','other',1,9,'test');",
                  0, 0, 0));
  const char *queries[] = {
      "SELECT jid,push_name,full_name,first_name,business_name FROM contacts "
      "WHERE device_id=? AND jid=?",
      "SELECT phone_number FROM lid_pn_mapping WHERE device_id=? AND lid=?",
      "SELECT lid FROM lid_pn_mapping WHERE device_id=? AND phone_number=? "
      "ORDER BY updated_at DESC,lid DESC"};
  printf("SQLite %s source %s\n", sqlite3_libversion(), sqlite3_sourceid());
  for (int q = 0; q < 3; q++)
    for (int sample = 0; sample < 8; sample++)
      for (int position = 0; position < 2; position++) {
        int bound = sample % 2 ? position : 1 - position;
        char sql[1024];
        snprintf(sql, sizeof sql, "%s LIMIT %s", queries[q], bound ? "?" : "1");
        char explain[1100];
        snprintf(explain, sizeof explain, "EXPLAIN QUERY PLAN %s", sql);
        sqlite3_stmt *s = 0;
        ok(sqlite3_prepare_v3(db, explain, -1, 0, &s, 0));
        bind(s, bound, 0);
        printf("plan q=%d bound=%d", q, bound);
        while (sqlite3_step(s) == SQLITE_ROW)
          printf(" | %s", sqlite3_column_text(s, 3));
        puts("");
        ok(sqlite3_finalize(s));

        long long start = ns(CLOCK_MONOTONIC);
        ok(sqlite3_prepare_v3(db, sql, -1, SQLITE_PREPARE_PERSISTENT, &s, 0));
        long long prepare = ns(CLOCK_MONOTONIC) - start;
        sqlite3_int64 before = sqlite3_memory_used();
        sqlite3_memory_highwater(1);
        allocations = bytes = 0;
        counting = getenv("POINT_C_ALLOC") != NULL;
        long long cpu = ns(CLOCK_PROCESS_CPUTIME_ID);
        start = ns(CLOCK_MONOTONIC);
        int reprep = 0;
        for (int i = 0; i < 20000; i++) {
          bind(s, bound, i % 2);
          int rc = sqlite3_step(s);
          if (rc != (i % 2 ? SQLITE_DONE : SQLITE_ROW))
            abort();
          if (rc == SQLITE_ROW) {
            const char *v =
                (const char *)sqlite3_column_text(s, q == 0 ? 2 : 0);
            const char *want = q == 0 ? "name" : q == 1 ? "synthetic" : "tie";
            if (!v || strcmp(v, want))
              abort();
          }
          reprep += sqlite3_stmt_status(s, SQLITE_STMTSTATUS_REPREPARE, 1);
          ok(sqlite3_reset(s));
          ok(sqlite3_clear_bindings(s));
        }
        counting = 0;
        long long wall = ns(CLOCK_MONOTONIC) - start;
        cpu = ns(CLOCK_PROCESS_CPUTIME_ID) - cpu;
        printf(
            "q=%d bound=%d sample=%d prepare_ns=%lld wall_ns=%lld cpu_ns=%lld "
            "reprepare=%d heap_before=%lld heap_after=%lld heap_peak=%lld "
            "stmt_bytes=%d alloc_calls=%lld alloc_bytes=%lld\n",
            q, bound, sample, prepare, wall, cpu, reprep, (long long)before,
            (long long)sqlite3_memory_used(),
            (long long)sqlite3_memory_highwater(0),
            sqlite3_stmt_status(s, SQLITE_STMTSTATUS_MEMUSED, 0), allocations,
            bytes);
        if (reprep != (bound ? 20000 : 0))
          abort();
        ok(sqlite3_finalize(s));
      }
  ok(sqlite3_close(db));
}
