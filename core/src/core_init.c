/*
  This file is appended to the end of a sqlite3.c amalgammation
  file to include crsqlite functions statically in
  a build. This is used for the demo CLI and WASM implementations.

  It registers the auto-extension entry point, so every open runs init under
  the Init Busy Timeout (docs/CRSQLITE_INIT_RACE_FIX.md).
*/
#include "ext.h"

int core_init(const char *dummy) {
  return sqlite3_auto_extension((void *)sqlite3_crsqlite_autoext_init);
}
