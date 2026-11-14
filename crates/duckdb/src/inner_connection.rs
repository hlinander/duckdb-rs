use std::{
    ffi::{CStr, CString},
    mem,
    os::raw::c_char,
    ptr, str,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use super::{Appender, Config, Connection, Result, ffi};
use crate::{
    error::{
        Error, error_from_duckdb_extract, result_from_duckdb_appender, result_from_duckdb_extract,
        result_from_duckdb_prepare, result_from_duckdb_result,
    },
    extracted_statements::ExtractedStatements,
    raw_statement::RawStatement,
    statement::Statement,
};

#[derive(Debug)]
struct DatabaseHandle {
    db: ffi::duckdb_database,
    close_on_drop: bool,
}

// `duckdb_database` is an opaque C pointer. We share it via `Arc` so the database
// is closed exactly once when the last connection referencing it is dropped.
// This means the handle may be dropped on any thread that owns a `Connection`.
unsafe impl Send for DatabaseHandle {}

impl DatabaseHandle {
    #[inline]
    pub fn new(db: ffi::duckdb_database, close_on_drop: bool) -> Self {
        Self { db, close_on_drop }
    }

    #[inline]
    pub fn raw(&self) -> ffi::duckdb_database {
        self.db
    }
}

impl Drop for DatabaseHandle {
    fn drop(&mut self) {
        if !self.close_on_drop {
            return;
        }
        if self.db.is_null() {
            return;
        }
        unsafe {
            ffi::duckdb_close(&mut self.db);
            self.db = ptr::null_mut();
        }
    }
}

pub struct InnerConnection {
    // Mutex makes the Send-only handle `Sync` so the `Arc` can be shared across threads;
    // it is not used for concurrent access to the database itself.
    database: Arc<Mutex<DatabaseHandle>>,
    pub con: ffi::duckdb_connection,
    interrupt: Arc<InterruptHandle>,
}

impl InnerConnection {
    #[inline]
    unsafe fn new(database: Arc<Mutex<DatabaseHandle>>) -> Result<Self> {
        unsafe {
            let mut con: ffi::duckdb_connection = ptr::null_mut();
            let db_raw = database.lock().expect("database handle mutex poisoned").raw();
            let r = ffi::duckdb_connect(db_raw, &mut con);
            if r != ffi::DuckDBSuccess {
                ffi::duckdb_disconnect(&mut con);
                return Err(Error::DuckDBFailure(
                    ffi::Error::new(r),
                    Some("connect error".to_owned()),
                ));
            }
            let interrupt = Arc::new(InterruptHandle::new(con));

            Ok(Self {
                database,
                con,
                interrupt,
            })
        }
    }

    pub fn open_with_flags(c_path: &CStr, config: Config) -> Result<Self> {
        unsafe {
            let mut db: ffi::duckdb_database = ptr::null_mut();
            let mut c_err = std::ptr::null_mut();
            let r = ffi::duckdb_open_ext(c_path.as_ptr(), &mut db, config.duckdb_config(), &mut c_err);
            if r != ffi::DuckDBSuccess {
                let msg = ffi::DuckDbString::from_nullable_ptr(c_err).map(|c_err| c_err.to_string_lossy().to_string());
                return Err(Error::DuckDBFailure(ffi::Error::new(r), msg));
            }
            Self::new_from_raw_db(db, true)
        }
    }

    #[inline]
    pub(crate) unsafe fn new_from_raw_db(raw: ffi::duckdb_database, close_on_drop: bool) -> Result<Self> {
        unsafe { Self::new(Arc::new(Mutex::new(DatabaseHandle::new(raw, close_on_drop)))) }
    }

    pub fn close(&mut self) -> Result<()> {
        if self.con.is_null() {
            return Ok(());
        }
        // Clear the handle's pointer first. `clear` waits for a reader that holds the handle's
        // mutex, and every later reader sees a null pointer, so no reader holds the native
        // connection once it is disconnected.
        self.interrupt.clear();
        unsafe {
            ffi::duckdb_disconnect(&mut self.con);
            self.con = ptr::null_mut();
        }
        Ok(())
    }

    /// Creates a new connection to the already-opened database.
    pub fn try_clone(&self) -> Result<Self> {
        unsafe { Self::new(self.database.clone()) }
    }

    pub fn execute(&mut self, sql: &str) -> Result<()> {
        let c_str = CString::new(sql)?;
        unsafe {
            let mut out = mem::zeroed();
            let r = ffi::duckdb_query(self.con, c_str.as_ptr() as *const c_char, &mut out);
            result_from_duckdb_result(r, &mut out)?;
            ffi::duckdb_destroy_result(&mut out);
            Ok(())
        }
    }

    pub fn prepare<'a>(&mut self, conn: &'a Connection, sql: &str) -> Result<Statement<'a>> {
        let c_str = CString::new(sql)?;

        // Extract statements (handles both single and multi-statement queries)
        let mut extracted = ptr::null_mut();
        let num_stmts =
            unsafe { ffi::duckdb_extract_statements(self.con, c_str.as_ptr() as *const c_char, &mut extracted) };
        result_from_duckdb_extract(num_stmts, extracted)?;

        // Auto-cleanup on drop
        let _guard = ExtractedStatementsGuard(extracted);

        // Execute all intermediate statements
        for i in 0..num_stmts - 1 {
            self.execute_extracted_statement(extracted, i)?;
        }

        // Prepare and return final statement
        let final_stmt = self.prepare_extracted_statement(extracted, num_stmts - 1)?;
        Ok(Statement::new(conn, unsafe { RawStatement::new(final_stmt) }))
    }

    pub(crate) fn prepare_extracted_statement(
        &self,
        extracted: ffi::duckdb_extracted_statements,
        index: ffi::idx_t,
    ) -> Result<ffi::duckdb_prepared_statement> {
        let mut stmt = ptr::null_mut();
        let res = unsafe { ffi::duckdb_prepare_extracted_statement(self.con, extracted, index, &mut stmt) };
        result_from_duckdb_prepare(res, stmt)?;
        Ok(stmt)
    }

    /// Runs the extracted statement at `index` and discards its result. No
    /// result column is inspected, because nothing decodes the result.
    pub(crate) fn execute_extracted_statement(
        &self,
        extracted: ffi::duckdb_extracted_statements,
        index: ffi::idx_t,
    ) -> Result<()> {
        let mut stmt = self.prepare_extracted_statement(extracted, index)?;

        unsafe {
            let mut result = mem::zeroed();
            let rc = ffi::duckdb_execute_prepared(stmt, &mut result);
            // Destroys the result when the execution failed.
            let outcome = result_from_duckdb_result(rc, &mut result);
            ffi::duckdb_destroy_prepare(&mut stmt);
            if outcome.is_ok() {
                ffi::duckdb_destroy_result(&mut result);
            }
            outcome
        }
    }

    /// Extracts the statements of `sql` without running any of them.
    pub fn extract<'a>(&mut self, conn: &'a Connection, sql: &str) -> Result<ExtractedStatements<'a>> {
        let c_str = CString::new(sql)?;

        let mut extracted = ptr::null_mut();
        let num_stmts =
            unsafe { ffi::duckdb_extract_statements(self.con, c_str.as_ptr() as *const c_char, &mut extracted) };

        // A block that holds no statement extracts to zero statements and no error.
        let failed = extracted.is_null()
            || (num_stmts == 0 && unsafe { !ffi::duckdb_extract_statements_error(extracted).is_null() });
        if failed {
            return Err(unsafe { error_from_duckdb_extract(extracted) });
        }

        Ok(unsafe { ExtractedStatements::new(conn, extracted, num_stmts as usize) })
    }

    pub fn appender<'a>(
        &mut self,
        conn: &'a Connection,
        catalog: Option<&str>,
        schema: &str,
        table: &str,
    ) -> Result<Appender<'a>> {
        let c_app = self.create_table_appender(catalog, schema, table)?;
        Ok(Appender::new(conn, c_app))
    }

    fn create_table_appender(
        &mut self,
        catalog: Option<&str>,
        schema: &str,
        table: &str,
    ) -> Result<ffi::duckdb_appender> {
        let mut c_app: ffi::duckdb_appender = ptr::null_mut();
        let c_catalog = catalog.map(CString::new).transpose()?;
        let c_table = CString::new(table)?;
        let c_schema = CString::new(schema)?;
        let r = unsafe {
            ffi::duckdb_appender_create_ext(
                self.con,
                c_catalog.as_ref().map_or(ptr::null(), |c| c.as_ptr() as *const c_char),
                c_schema.as_ptr() as *const c_char,
                c_table.as_ptr() as *const c_char,
                &mut c_app,
            )
        };
        result_from_duckdb_appender(r, &mut c_app)?;
        Ok(c_app)
    }

    pub fn appender_with_columns<'a>(
        &mut self,
        conn: &'a Connection,
        catalog: Option<&str>,
        schema: &str,
        table: &str,
        columns: &[&str],
    ) -> Result<Appender<'a>> {
        // The C API only supports narrowing columns after the appender is created.
        // Create the appender first, then activate the requested column subset.
        let c_app = self.create_table_appender(catalog, schema, table)?;
        let mut appender = Appender::new(conn, c_app);
        for column in columns {
            appender.add_column(column)?;
        }
        Ok(appender)
    }

    pub fn get_interrupt_handle(&self) -> Arc<InterruptHandle> {
        self.interrupt.clone()
    }

    #[inline]
    pub fn is_autocommit(&self) -> bool {
        true
    }
}

struct ExtractedStatementsGuard(ffi::duckdb_extracted_statements);

impl Drop for ExtractedStatementsGuard {
    fn drop(&mut self) {
        unsafe { ffi::duckdb_destroy_extracted(&mut self.0) }
    }
}

impl Drop for InnerConnection {
    #[inline]
    fn drop(&mut self) {
        use std::thread::panicking;
        if let Err(e) = self.close() {
            if panicking() {
                eprintln!("Error while closing DuckDB connection: {e:?}");
            } else {
                panic!("Error while closing DuckDB connection: {e:?}");
            }
        }
    }
}

/// A handle that allows interrupting long-running queries.
pub struct InterruptHandle {
    conn: Mutex<ffi::duckdb_connection>,
}

unsafe impl Send for InterruptHandle {}
unsafe impl Sync for InterruptHandle {}

impl InterruptHandle {
    fn new(conn: ffi::duckdb_connection) -> Self {
        Self { conn: Mutex::new(conn) }
    }

    fn clear(&self) {
        *self.lock() = ptr::null_mut();
    }

    // The stored pointer is valid whether or not the mutex is poisoned, so a panic in a
    // `with_connection` closure leaves the handle usable.
    fn lock(&self) -> MutexGuard<'_, ffi::duckdb_connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Interrupt the query currently running on the connection this handle was
    /// obtained from. The interrupt will cause that query to fail with
    /// `Error::DuckDBFailure`. If the connection was dropped after obtaining
    /// this interrupt handle, calling this method results in a noop.
    ///
    /// See [`crate::Connection::interrupt_handle`] for an example.
    pub fn interrupt(&self) {
        let db_handle = self.lock();

        if !db_handle.is_null() {
            unsafe {
                ffi::duckdb_interrupt(*db_handle);
            }
        }
    }

    /// Runs `f` with the native connection this handle was obtained from and
    /// returns its result, or returns `None` without running `f` once the
    /// connection is closed.
    ///
    /// `f` runs under the handle's mutex. Closing the connection takes that
    /// mutex before it disconnects, so the pointer is live for the whole of
    /// `f`. [`InterruptHandle::interrupt`] takes the same mutex and waits
    /// while `f` runs.
    ///
    /// The mutex is not re-entrant. `f` must not call this handle, and must
    /// not close or drop the connection: each of those takes the mutex that
    /// `f` runs under and blocks forever. A panic in `f` leaves the handle
    /// and the connection usable.
    ///
    /// The pointer is valid outside `f` only for a caller that otherwise
    /// prevents the connection from closing.
    pub fn with_connection<R>(&self, f: impl FnOnce(ffi::duckdb_connection) -> R) -> Option<R> {
        let db_handle = self.lock();

        if db_handle.is_null() { None } else { Some(f(*db_handle)) }
    }
}

#[cfg(test)]
mod test {
    use crate::{Connection, Result, ffi};
    use std::{
        mem,
        panic::{self, AssertUnwindSafe},
        sync::{
            Arc, Barrier,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    // The loadable-extension API struct does not carry `duckdb_resource_totals_read`.
    #[cfg(not(feature = "loadable-extension"))]
    #[test]
    fn with_connection_passes_the_native_connection() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let handle = db.interrupt_handle();

        let state = handle.with_connection(|con| unsafe {
            assert!(!con.is_null());
            let mut totals: ffi::duckdb_resource_totals = mem::zeroed();
            ffi::duckdb_resource_totals_read(con, &mut totals)
        });

        assert_eq!(state, Some(ffi::DuckDBSuccess));
        Ok(())
    }

    #[test]
    fn with_connection_returns_none_after_close() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let handle = db.interrupt_handle();
        db.close().map_err(|(_, err)| err)?;

        let mut ran = false;
        assert_eq!(handle.with_connection(|_| ran = true), None);
        assert!(!ran);
        Ok(())
    }

    #[test]
    fn with_connection_returns_none_after_drop() -> Result<()> {
        let handle = Connection::open_in_memory()?.interrupt_handle();

        assert!(handle.with_connection(|con| con).is_none());
        Ok(())
    }

    #[test]
    fn panic_in_reader_leaves_interrupt_and_close_usable() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let handle = db.interrupt_handle();

        let unwound = panic::catch_unwind(AssertUnwindSafe(|| {
            handle.with_connection(|_| panic!("reader panics under the mutex"));
        }));
        assert!(unwound.is_err());

        handle.interrupt();
        assert_eq!(handle.with_connection(|con| !con.is_null()), Some(true));
        db.execute_batch("SELECT 1")?;

        db.db.borrow_mut().close()?;
        assert!(db.db.borrow().con.is_null(), "close did not disconnect");
        assert!(handle.with_connection(|_| ()).is_none());
        handle.interrupt();
        Ok(())
    }

    #[test]
    fn panic_in_reader_leaves_drop_usable() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let handle = db.interrupt_handle();

        let unwound = panic::catch_unwind(AssertUnwindSafe(|| {
            handle.with_connection(|_| panic!("reader panics under the mutex"));
        }));
        assert!(unwound.is_err());

        drop(db);
        assert!(handle.with_connection(|_| ()).is_none());
        handle.interrupt();
        Ok(())
    }

    // The loadable-extension API struct does not carry `duckdb_resource_totals_read`.
    #[cfg(not(feature = "loadable-extension"))]
    #[test]
    fn close_waits_for_a_reader() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let handle = db.interrupt_handle();
        let entered = Arc::new(Barrier::new(2));
        let finished = Arc::new(AtomicBool::new(false));

        let reader = {
            let handle = handle.clone();
            let entered = entered.clone();
            let finished = finished.clone();
            thread::spawn(move || {
                handle.with_connection(|con| {
                    entered.wait();
                    thread::sleep(Duration::from_millis(200));
                    let mut totals: ffi::duckdb_resource_totals = unsafe { mem::zeroed() };
                    let state = unsafe { ffi::duckdb_resource_totals_read(con, &mut totals) };
                    finished.store(true, Ordering::SeqCst);
                    state
                })
            })
        };

        // The reader holds the mutex from here until it has made its engine call.
        entered.wait();
        db.close().map_err(|(_, err)| err)?;
        assert!(
            finished.load(Ordering::SeqCst),
            "close returned while a reader held the connection"
        );

        assert_eq!(reader.join().unwrap(), Some(ffi::DuckDBSuccess));
        assert!(handle.with_connection(|_| ()).is_none());
        Ok(())
    }
}
