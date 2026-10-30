/* Link against Cargo's bundled libsqlite3.a to inspect LIMIT specialization.
 * This runs cached statements on an empty synthetic table, not a benchmark. */
#include "sqlite3.h"
#include <stdio.h>
#include <stdlib.h>

static void check(int rc) {
    if (rc != SQLITE_OK) { fprintf(stderr, "SQLite status %d\n", rc); exit(1); }
}

int main(void) {
    sqlite3 *db = NULL;
    check(sqlite3_open(":memory:", &db));
    check(sqlite3_exec(db,
        "CREATE TABLE messages(id INTEGER PRIMARY KEY, device_id INTEGER,"
        "chat_jid TEXT, timestamp_ms BIGINT);"
        "CREATE INDEX idx_messages_chat_time ON messages(device_id,chat_jid,timestamp_ms);",
        NULL, NULL, NULL));
    const char *sql[] = {
        "SELECT * FROM messages WHERE device_id=? AND chat_jid IN (?) ORDER BY timestamp_ms DESC,id DESC LIMIT ?",
        "SELECT * FROM messages WHERE device_id=? AND chat_jid IN (?) ORDER BY timestamp_ms DESC,id DESC LIMIT CAST(? AS INTEGER)"
    };
    printf("SQLite %s\n", sqlite3_libversion());
    for (int candidate=0; candidate<2; candidate++) {
        sqlite3_stmt *stmt = NULL;
        check(sqlite3_prepare_v3(db, sql[candidate], -1, SQLITE_PREPARE_PERSISTENT, &stmt, NULL));
        printf("%s reprepare counts:", candidate ? "cast" : "bound");
        for (int iteration=0; iteration<5; iteration++) {
            check(sqlite3_bind_int(stmt,1,1));
            check(sqlite3_bind_text(stmt,2,"synthetic",-1,SQLITE_STATIC));
            check(sqlite3_bind_int64(stmt,3,8));
            int rc;
            while ((rc=sqlite3_step(stmt)) == SQLITE_ROW) {}
            if (rc != SQLITE_DONE) check(rc);
            int count=sqlite3_stmt_status(stmt,SQLITE_STMTSTATUS_REPREPARE,1);
            printf(" %d",count);
            if (count != (candidate ? 0 : 1)) {
                fprintf(stderr,"\nEngine behavior changed; remeasure before relying on CAST.\n");
                return 1;
            }
            check(sqlite3_reset(stmt));
        }
        puts("");
        check(sqlite3_finalize(stmt));
    }
    check(sqlite3_close(db));
    return 0;
}
