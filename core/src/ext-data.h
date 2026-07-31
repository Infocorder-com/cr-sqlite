#ifndef CRSQLITE_EXTDATA_H
#define CRSQLITE_EXTDATA_H

#include "sqlite3ext.h"
SQLITE_EXTENSION_INIT3

// NOTE: any changes here must be updated in `c.rs` until we've finished porting
// to rust.
typedef struct crsql_ExtData crsql_ExtData;
struct crsql_ExtData {
  // perma statement -- used to check db schema version
  sqlite3_stmt *pPragmaSchemaVersionStmt;
  sqlite3_stmt *pPragmaDataVersionStmt;
  int pragmaDataVersion;

  // this gets set at the start of each transaction on the first invocation
  // to crsql_next_db_version()
  // and re-set on transaction commit or rollback.
  sqlite3_int64 dbVersion;
  // the version that the db will be set to at the end of the transaction
  // if that transaction were to commit at the time this value is checked.
  sqlite3_int64 pendingDbVersion;

  int pragmaSchemaVersion;
  int updatedTableInfosThisTx;

  // we need another schema version number that tracks when we checked it
  // for zpTableInfos.
  int pragmaSchemaVersionForTableInfos;

  unsigned char *siteId;
  sqlite3_stmt *pDbVersionStmt;
  sqlite3_stmt *pSetDbVersionStmt;
  void *tableInfos;
  void *lastDbVersions;

  // tracks the number of rows impacted by all inserts into crsql_changes in the
  // current transaction. This number is reset on transaction commit.
  int rowsImpacted;

  int seq;

  sqlite3_stmt *pSetSyncBitStmt;
  sqlite3_stmt *pClearSyncBitStmt;
  sqlite3_stmt *pSetSiteIdOrdinalStmt;
  sqlite3_stmt *pSelectSiteIdOrdinalStmt;
  sqlite3_stmt *pSelectClockTablesStmt;

  int mergeEqualValues;
  unsigned long long timestamp;
  void *ordinalMap;

  // silicon_brain Approach B (lazy init): 0 until crsql_ensure_bootstrapped()
  // has created the bookkeeping tables/triggers AND crsql_finish_ext_data_init()
  // has prepared the table-dependent statements + read config. This lets
  // sqlite3_crsqlite_init() be side-effect-free at sqlite3_open(), so cr-sqlite
  // can be statically auto-registered on a page-encrypted DB whose key is only
  // set AFTER open. NOTE: mirrored in c.rs (and the layout test there).
  int bootstrapped;
};

crsql_ExtData *crsql_newExtData(sqlite3 *db);
// Deferred DB-touching half of ext-data init (prepares table-dependent
// statements + reads crsql_master config). Called lazily by
// crsql_ensure_bootstrapped after the bootstrap tables exist and the key is set.
int crsql_finish_ext_data_init(sqlite3 *db, crsql_ExtData *pExtData);
int crsql_initSiteIdExt(sqlite3 *db, crsql_ExtData *pExtData, unsigned char *siteIdBuffer);
void crsql_freeExtData(crsql_ExtData *pExtData);
int crsql_fetchPragmaSchemaVersion(sqlite3 *db, crsql_ExtData *pExtData,
                                   int which);
int crsql_fetchPragmaDataVersion(sqlite3 *db, crsql_ExtData *pExtData);
int crsql_recreate_db_version_stmt(sqlite3 *db, crsql_ExtData *pExtData);
void crsql_finalize(crsql_ExtData *pExtData);
void crsql_init_ordinal_map(crsql_ExtData *pExtData);
void crsql_drop_ordinal_map(crsql_ExtData *pExtData);

#endif
