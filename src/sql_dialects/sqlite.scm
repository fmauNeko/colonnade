; SQLite dialect
((identifier) @keyword (#match? @keyword "^(?i)(pragma|autoincrement|glob|attach|detach|reindex|abort|fail|rowid|without)$"))
((identifier) @type (#match? @type "^(?i)(blob|any)$"))
