; MySQL dialect
((identifier) @type (#match? @type "^(?i)(tinytext|mediumtext|longtext|tinyblob|mediumblob|longblob|year|linestring|polygon|multipoint|multilinestring|multipolygon|geometrycollection)$"))
((identifier) @keyword (#match? @keyword "^(?i)(regexp|straight_join|sql_calc_found_rows|describe|charset|lock|unlock|use|show|status|processlist)$"))
