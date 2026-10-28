; PostgreSQL dialect
((identifier) @type (#match? @type "^(?i)(bpchar|cidr|citext|daterange|float4|float8|hstore|int2|int4|int8|int4range|int8range|jsonpath|ltree|macaddr|macaddr8|numrange|pg_lsn|regconfig|tsquery|tsrange|tstzrange|tsvector|txid_snapshot|varbit|xid)$"))
((identifier) @keyword (#match? @keyword "^(?i)(ilike|notnull|isnull|analyse|tablesample|listen|notify|unlisten)$"))
