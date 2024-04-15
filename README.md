# Motivation
`pg_debug_scan` is a PostgreSQL extension that debugs table scans by proving custom snapshot definitions. This extension is intended to teach the internals of database systems and allow users to see the results of different snapshots and visibility rules. In addition, I needed a small project to explore writing PostgreSQL extensions in Rust with pgrx.

The foundations of snapshots can be found in this [article](https://jnidzwetzki.github.io/2024/04/03/postgres-and-snapshots.html). This extension provides the function `pg_debug_scan`, which takes a table name and the xmin, xmax, and xip values of a snapshot as arguments and performs a full table scan using this snapshot data.

## Build and Install 
```shell
cargo pgrx install
```

## Example 
```sql
CREATE EXTENSION pg_debug_scan;

CREATE TABLE temperature (
  time timestamptz NOT NULL,
  value float
);

INSERT INTO temperature VALUES(now(), 1);
INSERT INTO temperature VALUES(now(), 2);
INSERT INTO temperature VALUES(now(), 3);

BEGIN TRANSACTION;
DELETE FROM TEMPERATURE where value = 2;
SELECT * FROM txid_current_if_assigned();
 
 txid_current_if_assigned
--------------------------
                      774

COMMIT;

SELECT xmin, xmax, * FROM temperature;
 xmin | xmax |             time              | value
------+------+-------------------------------+-------
  771 |    0 | 2024-04-12 15:59:23.348272+02 |     1
  773 |    0 | 2024-04-12 15:59:23.362715+02 |     3
(2 rows)

-- Based on the output, we know that the first record should be visible
-- in all transactions with a txid >= 771. The second record if visible
-- for all txid => 773.
-- 
-- One record is deleted but we can assume it was created with a xmin
-- value of 772 and we know from the txid_current_if_assigned output,
-- it was deleted in the transaction with the id 774. 

-- If we use the same data as the snapshot of our session...
SELECT * FROM pg_current_snapshot();
 pg_current_snapshot
---------------------
 775:775:

-- .. the extension returns the same data as the regular SELECT
SELECT * from pg_debug_scan('temperature', '775:775:');

 xmin | xmax |                         data
------+------+------------------------------------------------------
  771 |    0 | {"time":"2024-04-12 15:59:23.348272+02","value":"1"}
  773 |    0 | {"time":"2024-04-12 15:59:23.362715+02","value":"3"}

-- However, if we exclude txid 775, the deleted tuple becomes visible again
SELECT * from pg_debug_scan('temperature', '774:774:');

 xmin | xmax |                         data
------+------+------------------------------------------------------
  771 |    0 | {"time":"2024-04-12 15:59:23.348272+02","value":"1"}
  772 |  774 | {"time":"2024-04-12 15:59:23.357605+02","value":"2"}
  773 |    0 | {"time":"2024-04-12 15:59:23.362715+02","value":"3"}

-- And if we go one transaction further back in time, the last insert becomes invisible
SELECT * from pg_debug_scan('temperature', '773:773:');

 xmin | xmax |                         data
------+------+------------------------------------------------------
  771 |    0 | {"time":"2024-04-12 15:59:23.348272+02","value":"1"}
  772 |  774 | {"time":"2024-04-12 15:59:23.357605+02","value":"2"}
```

