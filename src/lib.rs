use serde_json::{Map, Value};
use std::ffi::CStr;
use std::ffi::CString;
use std::mem::size_of;
use std::ptr;

use pgrx::{
    pg_sys::{
        palloc, uint32, AccessShareLock, GetLatestSnapshot, GetTransactionSnapshot, SnapshotData,
    },
    prelude::*,
};

pgrx::pg_module_magic!();

struct SnapshotArguments {
    xmin: uint32,
    xmax: uint32,
    xip: Vec<u32>,
}

/*
 * Parse the provided snapshot data by the user. For example 4:45:23,35
 * means xmin:xmax:xip1,xip2 .
 *
 * See the PostgreSQL documentation - pg_current_snapshot() for more information
 * about the meaning of these values.
 */
fn parse_snapshot_data(snapshot_str: &str) -> SnapshotArguments {
    let parts: Vec<&str> = snapshot_str.split(':').collect();

    if parts.len() != 3 {
        error!("Unable to parse snapshot data {snapshot_str}");
    }

    let xmin = parts[0].parse().expect("Unable to parse xmin value");
    let xmax = parts[1].parse().expect("Unable to parse xmax value");

    /* Parse xip members (2,3,54) */
    let mut xip_values = Vec::new();

    if !parts[2].is_empty() {
        for part in parts[2].split(',') {
            let xip_value: u32 = part.parse().expect("unable to parse xip member: {part}");

            /* From PostgreSQL code:
             * Note: all ids in xip[] satisfy xmin <= xip[i] < xmax
             */
            if xip_value >= xmin && xip_value < xmax {
                xip_values.push(xip_value)
            } else {
                error!("Xip value {xip_value} is outside of {xmin}..{xmax}")
            }
        }
    }

    SnapshotArguments {
        xmin,
        xmax,
        xip: xip_values,
    }
}

/*
 * Take the user provided snapshot data and return a PostgreSQL snapshot data structure
 */
unsafe fn get_snapshot_from_str(snapshot_str: &str) -> *mut SnapshotData {
    let snapshot_argument = parse_snapshot_data(snapshot_str);

    /* Get the latest snapshot as base */
    let latest_snapshot = GetLatestSnapshot();

    /* Take a copy of the snapshot */
    let scan_snapshot = palloc(size_of::<SnapshotData>()) as *mut SnapshotData;
    ptr::copy_nonoverlapping(latest_snapshot, scan_snapshot, 1);

    /* Modify the relevant values */
    (*scan_snapshot).copied = true;
    (*scan_snapshot).xmin = snapshot_argument.xmin;
    (*scan_snapshot).xmax = snapshot_argument.xmax;
    (*scan_snapshot).xip = palloc(snapshot_argument.xip.len() * size_of::<u32>()) as *mut u32;
    ptr::copy_nonoverlapping(
        snapshot_argument.xip.as_ptr(),
        (*scan_snapshot).xip,
        snapshot_argument.xip.len(),
    );
    (*scan_snapshot).xcnt = snapshot_argument.xip.len() as u32;

    scan_snapshot
}

/*
 * Custom implementation for HeapTupleHeaderGetXmax. This function is currently not defined in pgrx.
 */
#[inline(always)]
#[allow(non_snake_case)]
unsafe fn HeapTupleHeaderGetXmax(
    tup: *const pgrx::pg_sys::HeapTupleHeaderData,
) -> pgrx::pg_sys::TransactionId {
    unsafe {
        // SAFETY:  caller has asserted `tup` is a valid HeapTupleHeader pointer
        if pgrx::pg_sys::HeapTupleHeaderFrozen(tup) {
            pgrx::pg_sys::FrozenTransactionId
        } else {
            (*tup).t_choice.t_heap.t_xmax
        }
    }
}

#[pg_extern]
unsafe fn pg_debug_scan(
    table: &str,
    snapshot: default!(Option<&str>, "NULL"),
) -> TableIterator<'static, (name!(xmin, i64), name!(xmax, i64), name!(data, String))> {
    info!("Reading table {table}");

    let snapshot_data = match snapshot {
        Some(snapshot_data) => get_snapshot_from_str(snapshot_data),
        None => GetTransactionSnapshot(),
    };

    info!(
        "Snapshot is (xmin={}, xmax={}, xcnt={})",
        (*snapshot_data).xmin,
        (*snapshot_data).xmax,
        (*snapshot_data).xcnt
    );

    /* Convert the table name into a range var */
    let range_list: *mut pg_sys::List;
    let table_str = CString::new(table).expect("Unable to convert to string");

    #[cfg(any(feature = "pg12", feature = "pg13", feature = "pg14", feature = "pg15"))]
    {
        range_list = pg_sys::stringToQualifiedNameList(table_str.as_ptr());
    }
    #[cfg(feature = "pg16")]
    {
        range_list = pg_sys::stringToQualifiedNameList(table_str.as_ptr(), std::ptr::null_mut());
    }

    let rangevar = pg_sys::makeRangeVarFromNameList(range_list);

    /* Get the Oid of the table */
    let relid = pg_sys::RangeVarGetRelidExtended(
        rangevar,
        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
        0,
        None,
        std::ptr::null_mut(),
    );

    /* Preform the table scan */
    let table_rel = pg_sys::table_open(relid, AccessShareLock as i32);
    let slot = pg_sys::table_slot_create(table_rel, std::ptr::null_mut());

    let scan = pg_sys::heap_beginscan(
        table_rel,
        snapshot_data,
        0,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        0,
    );

    let mut results: Vec<(i64, i64, String)> = Vec::new();

    /* Build a result tuple for each scanned tupe */
    while pg_sys::heap_getnextslot(scan, pg_sys::ScanDirection_ForwardScanDirection, slot) {
        /* No Rust port for slot_getsysattr available, so use HeapTupleHeaderGetXmin on the heap tuple */
        let get_heap_tuple_fn = (*(*slot).tts_ops).get_heap_tuple.unwrap();
        let htup = get_heap_tuple_fn(slot);
        let tupdesc = (*slot).tts_tupleDescriptor;

        let xmin = pg_sys::HeapTupleHeaderGetXmin((*htup).t_data);
        let xmax = HeapTupleHeaderGetXmax((*htup).t_data);
        let json = slot_to_json(relid, htup, tupdesc);
        results.push((xmin.into(), xmax.into(), json));
    }

    pg_sys::heap_endscan(scan);
    pg_sys::ExecDropSingleTupleTableSlot(slot);
    pg_sys::table_close(table_rel, AccessShareLock as i32);

    TableIterator::new(results)
}

/*
 * Convert the given slot into a json string
 */
unsafe fn slot_to_json(
    relid: pgrx::pg_sys::Oid,
    htup: *mut pgrx::pg_sys::HeapTupleData,
    tupdesc: *mut pgrx::pg_sys::TupleDescData,
) -> String {
    /* Build output JSON */
    let mut map = Map::new();

    let nattrs = (*tupdesc).natts as usize;
    let attrs = (*tupdesc).attrs.as_slice(nattrs);

    for attr_form_data in attrs.iter().take(nattrs) {
        if attr_form_data.attisdropped {
            continue;
        }

        /* Since we perform a plain table scan, each attribute should belong to the same base relation */
        assert!(
            relid == attr_form_data.attrelid,
            "attr and base relation have a different Oids {relid} {}",
            attr_form_data.attrelid
        );

        let attno = attr_form_data.attnum;
        assert!(attno > 0, "invalid attr no found during scan {attno}");

        let mut isnull: bool = false;
        let attr = pg_sys::heap_getattr(htup, attno.into(), tupdesc, &mut isnull);

        let colname_ptr = pg_sys::get_attname(relid, attno, false);
        let colname = CStr::from_ptr(colname_ptr).to_str().unwrap().to_string();

        if !isnull {
            let mut typoutput = pgrx::pg_sys::Oid::default();
            let mut typvarlena: bool = false;

            pg_sys::getTypeOutputInfo(attr_form_data.atttypid, &mut typoutput, &mut typvarlena);
            let output_val = pg_sys::OidOutputFunctionCall(typoutput, attr);
            let output_str = std::ffi::CStr::from_ptr(output_val);

            map.insert(
                colname,
                Value::String(output_str.to_str().unwrap().to_string()),
            );
        } else {
            map.insert(colname, Value::String("NULL".to_string()));
        }
    }

    serde_json::to_string(&map).expect("unable to generate JSON")
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    #[allow(unused_imports)]
    use pgrx::prelude::*;

    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct TemperatureJSON {
        time: String,
        value: String,
    }

    /* Get the SQL code for the integration test */
    fn get_test_sql(attribute: &str, txid: i64) -> String {
        format!(
            "SELECT {} FROM pg_debug_scan('temperature', '{}:{}:');",
            attribute, txid, txid
        )
    }

    #[pgrx::pg_test]
    fn test_parse_snapshot_data() {
        pgrx::Spi::run("CREATE TABLE temperature (time timestamptz NOT NULL, value float);")
            .unwrap();
        pgrx::Spi::run("INSERT INTO temperature VALUES('2024-04-12 15:59:23+02', 1);").unwrap();
        let txid = pgrx::Spi::get_one::<i64>("SELECT * FROM txid_current();")
            .unwrap()
            .expect("unable to get txid");

        /* Test returned xmin value */
        let xmin = pgrx::Spi::get_one::<i64>(get_test_sql("xmin", txid).as_str()).unwrap();
        assert_eq!(xmin, Some(txid));

        /* Test returned xmax value */
        let xmax = pgrx::Spi::get_one::<i64>(get_test_sql("xmax", txid).as_str()).unwrap();
        assert_eq!(xmax, Some(0));

        /* Test returned xmax value */
        let json_value = pgrx::Spi::get_one::<String>(get_test_sql("data", txid).as_str())
            .unwrap()
            .expect("unable to get json output");
        let tuple_data: TemperatureJSON = serde_json::from_str(json_value.as_str())
            .expect("failed to parse json response from SPI");
        assert_eq!(tuple_data.time, "2024-04-12 13:59:23+00");
        assert_eq!(tuple_data.value, "1");
    }
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {
        // perform one-off initialization when the pg_test framework starts
    }

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // return any postgresql.conf settings that are required for your tests
        vec!["timezone = UTC"]
    }
}
