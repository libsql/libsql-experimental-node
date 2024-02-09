use neon::types::buffer::TypedArray;
use neon::types::JsPromise;
use neon::{prelude::*, types::JsBigInt};
use once_cell::sync::OnceCell;
use std::cell::RefCell;
use std::sync::Arc;
use tokio::{runtime::Runtime, sync::Mutex};
use tracing::trace;

fn runtime<'a, C: Context<'a>>(cx: &mut C) -> NeonResult<&'static Runtime> {
    static RUNTIME: OnceCell<Runtime> = OnceCell::new();

    RUNTIME
        .get_or_try_init(Runtime::new)
        .or_else(|err| cx.throw_error(&err.to_string()))
}

struct Database {
    db: Arc<Mutex<libsql::Database>>,
    conn: RefCell<Option<Arc<Mutex<libsql::Connection>>>>,
    default_safe_integers: RefCell<bool>,
}

impl Finalize for Database {}

impl Database {
    fn new(db: libsql::Database, conn: libsql::Connection) -> Self {
        Database {
            db: Arc::new(Mutex::new(db)),
            conn: RefCell::new(Some(Arc::new(Mutex::new(conn)))),
            default_safe_integers: RefCell::new(false),
        }
    }

    fn js_open(mut cx: FunctionContext) -> JsResult<JsBox<Database>> {
        let rt = runtime(&mut cx)?;
        let db_path = cx.argument::<JsString>(0)?.value(&mut cx);
        let auth_token = cx.argument::<JsString>(1)?.value(&mut cx);
        let encryption_key = cx.argument::<JsString>(2)?.value(&mut cx);
        let db = if is_remote_path(&db_path) {
            let version = version("remote");

            trace!("Opening remote database: {}", db_path);
            libsql::Database::open_remote_internal(db_path.clone(), auth_token, version)
        } else {
            let mut builder = libsql::Builder::new_local(&db_path);
            if !encryption_key.is_empty() {
                builder = builder.encryption_key(encryption_key);
            }
            rt.block_on(builder.build())
        }
        .or_else(|err| throw_libsql_error(&mut cx, err))?;
        let conn = db
            .connect()
            .or_else(|err| throw_libsql_error(&mut cx, err))?;
        let db = Database::new(db, conn);
        Ok(cx.boxed(db))
    }

    fn js_open_with_rpc_sync(mut cx: FunctionContext) -> JsResult<JsBox<Database>> {
        let db_path = cx.argument::<JsString>(0)?.value(&mut cx);
        let sync_url = cx.argument::<JsString>(1)?.value(&mut cx);
        let sync_auth = cx.argument::<JsString>(2)?.value(&mut cx);
        let encryption_key = cx.argument::<JsString>(3)?.value(&mut cx);
        let encryption_key = if encryption_key.is_empty() {
            None
        } else {
            Some(encryption_key.into())
        };

        let version = version("rpc");

        trace!(
            "Opening local database with sync: database = {}, URL = {}",
            db_path,
            sync_url
        );
        let rt = runtime(&mut cx)?;
        let fut = libsql::Database::open_with_remote_sync_internal(
            db_path,
            sync_url,
            sync_auth,
            Some(version),
            true,
            encryption_key,
            None,
        );
        let result = rt.block_on(fut);
        let db = result.or_else(|err| cx.throw_error(err.to_string()))?;
        let conn = db
            .connect()
            .or_else(|err| throw_libsql_error(&mut cx, err))?;
        let db = Database::new(db, conn);
        Ok(cx.boxed(db))
    }

    fn js_in_transaction(mut cx: FunctionContext) -> JsResult<JsValue> {
        let db = cx.argument::<JsBox<Database>>(0)?;
        let conn = db.conn.borrow();
        let conn = conn.as_ref().unwrap().clone();
        let result = !conn.blocking_lock().is_autocommit();
        Ok(cx.boolean(result).upcast())
    }

    fn js_close(mut cx: FunctionContext) -> JsResult<JsUndefined> {
        // the conn will be closed when the last statement in discarded. In most situation that
        // means immediately because you don't want to hold on a statement for longer that its
        // database is alive.
        trace!("Closing database");
        let db: Handle<'_, JsBox<Database>> = cx.this()?;
        db.conn.replace(None);
        Ok(cx.undefined())
    }

    fn js_sync_sync(mut cx: FunctionContext) -> JsResult<JsUndefined> {
        trace!("Synchronizing database (sync)");
        let db: Handle<'_, JsBox<Database>> = cx.this()?;
        let db = db.db.clone();
        let rt = runtime(&mut cx)?;
        rt.block_on(async move {
            let db = db.lock().await;
            db.sync().await
        })
        .or_else(|err| throw_libsql_error(&mut cx, err))?;
        Ok(cx.undefined())
    }

    fn js_sync_async(mut cx: FunctionContext) -> JsResult<JsPromise> {
        trace!("Synchronizing database (async)");
        let db: Handle<'_, JsBox<Database>> = cx.this()?;
        let (deferred, promise) = cx.promise();
        let channel = cx.channel();
        let db = db.db.clone();
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            let result = db.lock().await.sync().await;
            match result {
                Ok(_) => {
                    deferred.settle_with(&channel, |mut cx| Ok(cx.undefined()));
                }
                Err(err) => {
                    deferred.settle_with(&channel, |mut cx| {
                        throw_libsql_error(&mut cx, err)?;
                        Ok(cx.undefined())
                    });
                }
            }
        });
        Ok(promise)
    }

    fn js_exec_sync(mut cx: FunctionContext) -> JsResult<JsUndefined> {
        let db: Handle<'_, JsBox<Database>> = cx.this()?;
        let sql = cx.argument::<JsString>(0)?.value(&mut cx);
        trace!("Executing SQL statement (sync): {}", sql);
        let conn = db.get_conn();
        let rt = runtime(&mut cx)?;
        let result = rt.block_on(async { conn.lock().await.execute_batch(&sql).await });
        result.or_else(|err| throw_libsql_error(&mut cx, err))?;
        Ok(cx.undefined())
    }

    fn js_exec_async(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let db: Handle<'_, JsBox<Database>> = cx.this()?;
        let sql = cx.argument::<JsString>(0)?.value(&mut cx);
        trace!("Executing SQL statement (async): {}", sql);
        let (deferred, promise) = cx.promise();
        let channel = cx.channel();
        let conn = db.get_conn();
        let rt = runtime(&mut cx)?;
        rt.spawn(async move {
            match conn.lock().await.execute_batch(&sql).await {
                Ok(_) => {
                    deferred.settle_with(&channel, |mut cx| Ok(cx.undefined()));
                }
                Err(err) => {
                    deferred.settle_with(&channel, |mut cx| {
                        throw_libsql_error(&mut cx, err)?;
                        Ok(cx.undefined())
                    });
                }
            }
        });
        Ok(promise)
    }

    fn js_prepare_sync<'a>(mut cx: FunctionContext) -> JsResult<JsBox<Statement>> {
        let db: Handle<'_, JsBox<Database>> = cx.this()?;
        let sql = cx.argument::<JsString>(0)?.value(&mut cx);
        trace!("Preparing SQL statement (sync): {}", sql);
        let conn = db.get_conn();
        let rt = runtime(&mut cx)?;
        let result = rt.block_on(async { conn.lock().await.prepare(&sql).await });
        let stmt = result.or_else(|err| throw_libsql_error(&mut cx, err))?;
        let stmt = Arc::new(Mutex::new(stmt));
        let stmt = Statement {
            conn: conn.clone(),
            stmt,
            raw: RefCell::new(false),
            safe_ints: RefCell::new(*db.default_safe_integers.borrow()),
        };
        Ok(cx.boxed(stmt))
    }

    fn js_prepare_async<'a>(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let db: Handle<'_, JsBox<Database>> = cx.this()?;
        let sql = cx.argument::<JsString>(0)?.value(&mut cx);
        trace!("Preparing SQL statement (async): {}", sql);
        let (deferred, promise) = cx.promise();
        let channel = cx.channel();
        let safe_ints = *db.default_safe_integers.borrow();
        let rt = runtime(&mut cx)?;
        let conn = db.get_conn();
        rt.spawn(async move {
            match conn.lock().await.prepare(&sql).await {
                Ok(stmt) => {
                    let stmt = Arc::new(Mutex::new(stmt));
                    let stmt = Statement {
                        conn: conn.clone(),
                        stmt,
                        raw: RefCell::new(false),
                        safe_ints: RefCell::new(safe_ints),
                    };
                    deferred.settle_with(&channel, |mut cx| Ok(cx.boxed(stmt)));
                }
                Err(err) => {
                    deferred.settle_with(&channel, |mut cx| {
                        throw_libsql_error(&mut cx, err)?;
                        Ok(cx.undefined())
                    });
                }
            }
        });
        Ok(promise)
    }

    fn js_default_safe_integers(mut cx: FunctionContext) -> JsResult<JsNull> {
        let db: Handle<'_, JsBox<Database>> = cx.this()?;
        let toggle = cx.argument::<JsBoolean>(0)?;
        let toggle = toggle.value(&mut cx);
        db.set_default_safe_integers(toggle);
        Ok(cx.null())
    }

    fn set_default_safe_integers(&self, toggle: bool) {
        self.default_safe_integers.replace(toggle);
    }

    fn get_conn(&self) -> Arc<Mutex<libsql::Connection>> {
        let conn = self.conn.borrow();
        conn.as_ref().unwrap().clone()
    }
}

fn is_remote_path(path: &str) -> bool {
    path.starts_with("libsql://") || path.starts_with("http://") || path.starts_with("https://")
}

fn throw_libsql_error<'a, C: Context<'a>, T>(cx: &mut C, err: libsql::Error) -> NeonResult<T> {
    match err {
        libsql::Error::SqliteFailure(code, err) => {
            let err = err.to_string();
            let err = JsError::error(cx, err).unwrap();
            let code_num = cx.number(code);
            err.set(cx, "rawCode", code_num).unwrap();
            let code = cx.string(convert_sqlite_code(code).to_string());
            err.set(cx, "code", code).unwrap();
            cx.throw(err)?
        }
        _ => {
            let err = err.to_string();
            let err = JsError::error(cx, err).unwrap();
            let code = cx.string("".to_string());
            err.set(cx, "code", code).unwrap();
            cx.throw(err)?
        }
    }
}

pub fn convert_sqlite_code(code: i32) -> String {
    match code {
        libsql::ffi::SQLITE_OK => "SQLITE_OK".to_owned(),
        libsql::ffi::SQLITE_ERROR => "SQLITE_ERROR".to_owned(),
        libsql::ffi::SQLITE_INTERNAL => "SQLITE_INTERNAL".to_owned(),
        libsql::ffi::SQLITE_PERM => "SQLITE_PERM".to_owned(),
        libsql::ffi::SQLITE_ABORT => "SQLITE_ABORT".to_owned(),
        libsql::ffi::SQLITE_BUSY => "SQLITE_BUSY".to_owned(),
        libsql::ffi::SQLITE_LOCKED => "SQLITE_LOCKED".to_owned(),
        libsql::ffi::SQLITE_NOMEM => "SQLITE_NOMEM".to_owned(),
        libsql::ffi::SQLITE_READONLY => "SQLITE_READONLY".to_owned(),
        libsql::ffi::SQLITE_INTERRUPT => "SQLITE_INTERRUPT".to_owned(),
        libsql::ffi::SQLITE_IOERR => "SQLITE_IOERR".to_owned(),
        libsql::ffi::SQLITE_CORRUPT => "SQLITE_CORRUPT".to_owned(),
        libsql::ffi::SQLITE_NOTFOUND => "SQLITE_NOTFOUND".to_owned(),
        libsql::ffi::SQLITE_FULL => "SQLITE_FULL".to_owned(),
        libsql::ffi::SQLITE_CANTOPEN => "SQLITE_CANTOPEN".to_owned(),
        libsql::ffi::SQLITE_PROTOCOL => "SQLITE_PROTOCOL".to_owned(),
        libsql::ffi::SQLITE_EMPTY => "SQLITE_EMPTY".to_owned(),
        libsql::ffi::SQLITE_SCHEMA => "SQLITE_SCHEMA".to_owned(),
        libsql::ffi::SQLITE_TOOBIG => "SQLITE_TOOBIG".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT => "SQLITE_CONSTRAINT".to_owned(),
        libsql::ffi::SQLITE_MISMATCH => "SQLITE_MISMATCH".to_owned(),
        libsql::ffi::SQLITE_MISUSE => "SQLITE_MISUSE".to_owned(),
        libsql::ffi::SQLITE_NOLFS => "SQLITE_NOLFS".to_owned(),
        libsql::ffi::SQLITE_AUTH => "SQLITE_AUTH".to_owned(),
        libsql::ffi::SQLITE_FORMAT => "SQLITE_FORMAT".to_owned(),
        libsql::ffi::SQLITE_RANGE => "SQLITE_RANGE".to_owned(),
        libsql::ffi::SQLITE_NOTADB => "SQLITE_NOTADB".to_owned(),
        libsql::ffi::SQLITE_NOTICE => "SQLITE_NOTICE".to_owned(),
        libsql::ffi::SQLITE_WARNING => "SQLITE_WARNING".to_owned(),
        libsql::ffi::SQLITE_ROW => "SQLITE_ROW".to_owned(),
        libsql::ffi::SQLITE_DONE => "SQLITE_DONE".to_owned(),
        libsql::ffi::SQLITE_IOERR_READ => "SQLITE_IOERR_READ".to_owned(),
        libsql::ffi::SQLITE_IOERR_SHORT_READ => "SQLITE_IOERR_SHORT_READ".to_owned(),
        libsql::ffi::SQLITE_IOERR_WRITE => "SQLITE_IOERR_WRITE".to_owned(),
        libsql::ffi::SQLITE_IOERR_FSYNC => "SQLITE_IOERR_FSYNC".to_owned(),
        libsql::ffi::SQLITE_IOERR_DIR_FSYNC => "SQLITE_IOERR_DIR_FSYNC".to_owned(),
        libsql::ffi::SQLITE_IOERR_TRUNCATE => "SQLITE_IOERR_TRUNCATE".to_owned(),
        libsql::ffi::SQLITE_IOERR_FSTAT => "SQLITE_IOERR_FSTAT".to_owned(),
        libsql::ffi::SQLITE_IOERR_UNLOCK => "SQLITE_IOERR_UNLOCK".to_owned(),
        libsql::ffi::SQLITE_IOERR_RDLOCK => "SQLITE_IOERR_RDLOCK".to_owned(),
        libsql::ffi::SQLITE_IOERR_DELETE => "SQLITE_IOERR_DELETE".to_owned(),
        libsql::ffi::SQLITE_IOERR_BLOCKED => "SQLITE_IOERR_BLOCKED".to_owned(),
        libsql::ffi::SQLITE_IOERR_NOMEM => "SQLITE_IOERR_NOMEM".to_owned(),
        libsql::ffi::SQLITE_IOERR_ACCESS => "SQLITE_IOERR_ACCESS".to_owned(),
        libsql::ffi::SQLITE_IOERR_CHECKRESERVEDLOCK => "SQLITE_IOERR_CHECKRESERVEDLOCK".to_owned(),
        libsql::ffi::SQLITE_IOERR_LOCK => "SQLITE_IOERR_LOCK".to_owned(),
        libsql::ffi::SQLITE_IOERR_CLOSE => "SQLITE_IOERR_CLOSE".to_owned(),
        libsql::ffi::SQLITE_IOERR_DIR_CLOSE => "SQLITE_IOERR_DIR_CLOSE".to_owned(),
        libsql::ffi::SQLITE_IOERR_SHMOPEN => "SQLITE_IOERR_SHMOPEN".to_owned(),
        libsql::ffi::SQLITE_IOERR_SHMSIZE => "SQLITE_IOERR_SHMSIZE".to_owned(),
        libsql::ffi::SQLITE_IOERR_SHMLOCK => "SQLITE_IOERR_SHMLOCK".to_owned(),
        libsql::ffi::SQLITE_IOERR_SHMMAP => "SQLITE_IOERR_SHMMAP".to_owned(),
        libsql::ffi::SQLITE_IOERR_SEEK => "SQLITE_IOERR_SEEK".to_owned(),
        libsql::ffi::SQLITE_IOERR_DELETE_NOENT => "SQLITE_IOERR_DELETE_NOENT".to_owned(),
        libsql::ffi::SQLITE_IOERR_MMAP => "SQLITE_IOERR_MMAP".to_owned(),
        libsql::ffi::SQLITE_IOERR_GETTEMPPATH => "SQLITE_IOERR_GETTEMPPATH".to_owned(),
        libsql::ffi::SQLITE_IOERR_CONVPATH => "SQLITE_IOERR_CONVPATH".to_owned(),
        libsql::ffi::SQLITE_IOERR_VNODE => "SQLITE_IOERR_VNODE".to_owned(),
        libsql::ffi::SQLITE_IOERR_AUTH => "SQLITE_IOERR_AUTH".to_owned(),
        libsql::ffi::SQLITE_LOCKED_SHAREDCACHE => "SQLITE_LOCKED_SHAREDCACHE".to_owned(),
        libsql::ffi::SQLITE_BUSY_RECOVERY => "SQLITE_BUSY_RECOVERY".to_owned(),
        libsql::ffi::SQLITE_BUSY_SNAPSHOT => "SQLITE_BUSY_SNAPSHOT".to_owned(),
        libsql::ffi::SQLITE_CANTOPEN_NOTEMPDIR => "SQLITE_CANTOPEN_NOTEMPDIR".to_owned(),
        libsql::ffi::SQLITE_CANTOPEN_ISDIR => "SQLITE_CANTOPEN_ISDIR".to_owned(),
        libsql::ffi::SQLITE_CANTOPEN_FULLPATH => "SQLITE_CANTOPEN_FULLPATH".to_owned(),
        libsql::ffi::SQLITE_CANTOPEN_CONVPATH => "SQLITE_CANTOPEN_CONVPATH".to_owned(),
        libsql::ffi::SQLITE_CORRUPT_VTAB => "SQLITE_CORRUPT_VTAB".to_owned(),
        libsql::ffi::SQLITE_READONLY_RECOVERY => "SQLITE_READONLY_RECOVERY".to_owned(),
        libsql::ffi::SQLITE_READONLY_CANTLOCK => "SQLITE_READONLY_CANTLOCK".to_owned(),
        libsql::ffi::SQLITE_READONLY_ROLLBACK => "SQLITE_READONLY_ROLLBACK".to_owned(),
        libsql::ffi::SQLITE_READONLY_DBMOVED => "SQLITE_READONLY_DBMOVED".to_owned(),
        libsql::ffi::SQLITE_ABORT_ROLLBACK => "SQLITE_ABORT_ROLLBACK".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT_CHECK => "SQLITE_CONSTRAINT_CHECK".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT_COMMITHOOK => "SQLITE_CONSTRAINT_COMMITHOOK".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => "SQLITE_CONSTRAINT_FOREIGNKEY".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT_FUNCTION => "SQLITE_CONSTRAINT_FUNCTION".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT_NOTNULL => "SQLITE_CONSTRAINT_NOTNULL".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => "SQLITE_CONSTRAINT_PRIMARYKEY".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT_TRIGGER => "SQLITE_CONSTRAINT_TRIGGER".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT_UNIQUE => "SQLITE_CONSTRAINT_UNIQUE".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT_VTAB => "SQLITE_CONSTRAINT_VTAB".to_owned(),
        libsql::ffi::SQLITE_CONSTRAINT_ROWID => "SQLITE_CONSTRAINT_ROWID".to_owned(),
        libsql::ffi::SQLITE_NOTICE_RECOVER_WAL => "SQLITE_NOTICE_RECOVER_WAL".to_owned(),
        libsql::ffi::SQLITE_NOTICE_RECOVER_ROLLBACK => "SQLITE_NOTICE_RECOVER_ROLLBACK".to_owned(),
        libsql::ffi::SQLITE_WARNING_AUTOINDEX => "SQLITE_WARNING_AUTOINDEX".to_owned(),
        libsql::ffi::SQLITE_AUTH_USER => "SQLITE_AUTH_USER".to_owned(),
        libsql::ffi::SQLITE_OK_LOAD_PERMANENTLY => "SQLITE_OK_LOAD_PERMANENTLY".to_owned(),
        _ => format!("UNKNOWN_SQLITE_ERROR_{}", code),
    }
}
struct Statement {
    conn: Arc<Mutex<libsql::Connection>>,
    stmt: Arc<Mutex<libsql::Statement>>,
    raw: RefCell<bool>,
    safe_ints: RefCell<bool>,
}

impl<'a> Finalize for Statement {}

fn js_value_to_value(
    cx: &mut FunctionContext,
    v: Handle<'_, JsValue>,
) -> NeonResult<libsql::Value> {
    if v.is_a::<JsNull, _>(cx) {
        Ok(libsql::Value::Null)
    } else if v.is_a::<JsUndefined, _>(cx) {
        Ok(libsql::Value::Null)
    } else if v.is_a::<JsArray, _>(cx) {
        todo!("array");
    } else if v.is_a::<JsBoolean, _>(cx) {
        todo!("bool");
    } else if v.is_a::<JsNumber, _>(cx) {
        let v = v.downcast_or_throw::<JsNumber, _>(cx)?;
        let v = v.value(cx);
        Ok(libsql::Value::Real(v))
    } else if v.is_a::<JsString, _>(cx) {
        let v = v.downcast_or_throw::<JsString, _>(cx)?;
        let v = v.value(cx);
        Ok(libsql::Value::Text(v))
    } else if v.is_a::<JsBigInt, _>(cx) {
        let v = v.downcast_or_throw::<JsBigInt, _>(cx)?;
        let v = v.to_i64(cx).or_throw(cx)?;
        Ok(libsql::Value::Integer(v))
    } else if v.is_a::<JsUint8Array, _>(cx) {
        let v = v.downcast_or_throw::<JsUint8Array, _>(cx)?;
        let v = v.as_slice(cx);
        Ok(libsql::Value::Blob(v.to_vec()))
    } else {
        todo!("unsupported type");
    }
}

impl Statement {
    fn js_raw(mut cx: FunctionContext) -> JsResult<JsNull> {
        let stmt: Handle<'_, JsBox<Statement>> = cx.this()?;
        let raw_stmt = stmt.stmt.blocking_lock();
        if raw_stmt.columns().is_empty() {
            return cx.throw_error("The raw() method is only for statements that return data");
        }
        let raw = cx.argument::<JsBoolean>(0)?;
        let raw = raw.value(&mut cx);
        stmt.set_raw(raw);
        Ok(cx.null())
    }

    fn set_raw(&self, raw: bool) {
        self.raw.replace(raw);
    }

    fn js_run(mut cx: FunctionContext) -> JsResult<JsValue> {
        let stmt: Handle<'_, JsBox<Statement>> = cx.this()?;
        let params = cx.argument::<JsValue>(0)?;
        let params = convert_params(&mut cx, &stmt, params)?;
        let mut raw_stmt = stmt.stmt.blocking_lock();
        raw_stmt.reset();
        let fut = raw_stmt.execute(params);
        let rt = runtime(&mut cx)?;
        let result = rt.block_on(fut);
        let changes = result.or_else(|err| throw_libsql_error(&mut cx, err))?;
        let raw_conn = stmt.conn.clone();
        let last_insert_rowid = raw_conn.blocking_lock().last_insert_rowid();
        let info = cx.empty_object();
        let changes = cx.number(changes as f64);
        info.set(&mut cx, "changes", changes)?;
        let last_insert_row_id = cx.number(last_insert_rowid as f64);
        info.set(&mut cx, "lastInsertRowid", last_insert_row_id)?;
        Ok(info.upcast())
    }

    fn js_get(mut cx: FunctionContext) -> JsResult<JsValue> {
        let stmt: Handle<'_, JsBox<Statement>> = cx.this()?;
        let params = cx.argument::<JsValue>(0)?;
        let params = convert_params(&mut cx, &stmt, params)?;
        let safe_ints = *stmt.safe_ints.borrow();
        let mut raw_stmt = stmt.stmt.blocking_lock();
        let fut = raw_stmt.query(params);
        let rt = runtime(&mut cx)?;
        let result = rt.block_on(fut);
        let mut rows = result.or_else(|err| throw_libsql_error(&mut cx, err))?;
        let result = rt
            .block_on(rows.next())
            .or_else(|err| throw_libsql_error(&mut cx, err))?;
        let result = match result {
            Some(row) => {
                if *stmt.raw.borrow() {
                    let mut result = cx.empty_array();
                    convert_row_raw(&mut cx, safe_ints, &mut result, &rows, &row)?;
                    Ok(result.upcast())
                } else {
                    let mut result = cx.empty_object();
                    convert_row(&mut cx, safe_ints, &mut result, &rows, &row)?;
                    Ok(result.upcast())
                }
            }
            None => Ok(cx.undefined().upcast()),
        };
        raw_stmt.reset();
        result
    }

    fn js_rows_sync(mut cx: FunctionContext) -> JsResult<JsValue> {
        let stmt: Handle<'_, JsBox<Statement>> = cx.this()?;
        let params = cx.argument::<JsValue>(0)?;
        let params = convert_params(&mut cx, &stmt, params)?;
        let rt = runtime(&mut cx)?;
        let result = rt.block_on(async move {
            let mut raw_stmt = stmt.stmt.lock().await;
            raw_stmt.reset();
            raw_stmt.query(params).await
        });
        let rows = result.or_else(|err| throw_libsql_error(&mut cx, err))?;
        let rows = Rows {
            rows: RefCell::new(rows),
            raw: *stmt.raw.borrow(),
            safe_ints: *stmt.safe_ints.borrow(),
        };
        Ok(cx.boxed(rows).upcast())
    }

    fn js_rows_async(mut cx: FunctionContext) -> JsResult<JsPromise> {
        let stmt: Handle<'_, JsBox<Statement>> = cx.this()?;
        let params = cx.argument::<JsValue>(0)?;
        let params = convert_params(&mut cx, &stmt, params)?;
        {
            let mut raw_stmt = stmt.stmt.blocking_lock();
            raw_stmt.reset();
        }
        let (deferred, promise) = cx.promise();
        let channel = cx.channel();
        let rt = runtime(&mut cx)?;
        let raw = *stmt.raw.borrow();
        let safe_ints = *stmt.safe_ints.borrow();
        let raw_stmt = stmt.stmt.clone();
        rt.spawn(async move {
            let result = {
                let mut raw_stmt = raw_stmt.lock().await;
                raw_stmt.query(params).await
            };
            match result {
                Ok(rows) => {
                    deferred.settle_with(&channel, move |mut cx| {
                        let rows = Rows {
                            rows: RefCell::new(rows),
                            raw,
                            safe_ints,
                        };
                        Ok(cx.boxed(rows))
                    });
                }
                Err(err) => {
                    deferred.settle_with(&channel, |mut cx| {
                        throw_libsql_error(&mut cx, err)?;
                        Ok(cx.undefined())
                    });
                }
            }
        });
        Ok(promise)
    }

    fn js_columns(mut cx: FunctionContext) -> JsResult<JsValue> {
        let stmt: Handle<'_, JsBox<Statement>> = cx.this()?;
        let result = cx.empty_array();
        let raw_stmt = stmt.stmt.blocking_lock();
        for (i, col) in raw_stmt.columns().iter().enumerate() {
            let column = cx.empty_object();
            let column_name = cx.string(col.name());
            column.set(&mut cx, "name", column_name)?;
            let column_origin_name: Handle<'_, JsValue> =
                if let Some(origin_name) = col.origin_name() {
                    cx.string(origin_name).upcast()
                } else {
                    cx.null().upcast()
                };
            column.set(&mut cx, "column", column_origin_name)?;
            let column_table_name: Handle<'_, JsValue> = if let Some(table_name) = col.table_name()
            {
                cx.string(table_name).upcast()
            } else {
                cx.null().upcast()
            };
            column.set(&mut cx, "table", column_table_name)?;
            let column_database_name: Handle<'_, JsValue> =
                if let Some(database_name) = col.database_name() {
                    cx.string(database_name).upcast()
                } else {
                    cx.null().upcast()
                };
            column.set(&mut cx, "database", column_database_name)?;
            let column_decl_type: Handle<'_, JsValue> = if let Some(decl_type) = col.decl_type() {
                cx.string(decl_type).upcast()
            } else {
                cx.null().upcast()
            };
            column.set(&mut cx, "type", column_decl_type)?;
            result.set(&mut cx, i as u32, column)?;
        }
        Ok(result.upcast())
    }

    fn js_safe_integers(mut cx: FunctionContext) -> JsResult<JsNull> {
        let stmt: Handle<'_, JsBox<Statement>> = cx.this()?;
        let toggle = cx.argument::<JsBoolean>(0)?;
        let toggle = toggle.value(&mut cx);
        stmt.set_safe_integers(toggle);
        Ok(cx.null())
    }

    fn set_safe_integers(&self, toggle: bool) {
        self.safe_ints.replace(toggle);
    }
}

struct Rows {
    rows: RefCell<libsql::Rows>,
    raw: bool,
    safe_ints: bool,
}

impl Finalize for Rows {}

impl Rows {
    fn js_next(mut cx: FunctionContext) -> JsResult<JsValue> {
        let rows: Handle<'_, JsBox<Rows>> = cx.this()?;
        let raw = rows.raw;
        let safe_ints = rows.safe_ints;
        let mut rows = rows.rows.borrow_mut();
        let rt = runtime(&mut cx)?;
        let next = rt
            .block_on(rows.next())
            .or_else(|err| throw_libsql_error(&mut cx, err))?;
        match next {
            Some(row) => {
                if raw {
                    let mut result = cx.empty_array();
                    convert_row_raw(&mut cx, safe_ints, &mut result, &rows, &row)?;
                    Ok(result.upcast())
                } else {
                    let mut result = cx.empty_object();
                    convert_row(&mut cx, safe_ints, &mut result, &rows, &row)?;
                    Ok(result.upcast())
                }
            }
            None => Ok(cx.undefined().upcast()),
        }
    }
}

fn convert_params(
    cx: &mut FunctionContext,
    stmt: &Statement,
    v: Handle<'_, JsValue>,
) -> NeonResult<libsql::params::Params> {
    if v.is_a::<JsArray, _>(cx) {
        let v = v.downcast_or_throw::<JsArray, _>(cx)?;
        convert_params_array(cx, v)
    } else {
        let v = v.downcast_or_throw::<JsObject, _>(cx)?;
        convert_params_object(cx, stmt, v)
    }
}

fn convert_params_array(
    cx: &mut FunctionContext,
    v: Handle<'_, JsArray>,
) -> NeonResult<libsql::params::Params> {
    let mut params = vec![];
    for i in 0..v.len(cx) {
        let v = v.get(cx, i)?;
        let v = js_value_to_value(cx, v)?;
        params.push(v);
    }
    Ok(libsql::params::Params::Positional(params))
}

fn convert_params_object(
    cx: &mut FunctionContext,
    stmt: &Statement,
    v: Handle<'_, JsObject>,
) -> NeonResult<libsql::params::Params> {
    let mut params = vec![];
    let stmt = &stmt.stmt;
    let raw_stmt = stmt.blocking_lock();
    for idx in 0..raw_stmt.parameter_count() {
        let name = raw_stmt.parameter_name((idx + 1) as i32).unwrap();
        let name = name.to_string();
        let v = v.get(cx, &name[1..])?;
        let v = js_value_to_value(cx, v)?;
        params.push((name, v));
    }
    Ok(libsql::params::Params::Named(params))
}

fn convert_row(
    cx: &mut FunctionContext,
    safe_ints: bool,
    result: &mut JsObject,
    rows: &libsql::Rows,
    row: &libsql::Row,
) -> NeonResult<()> {
    for idx in 0..rows.column_count() {
        let v = row
            .get_value(idx)
            .or_else(|err| throw_libsql_error(cx, err))?;
        let column_name = rows.column_name(idx).unwrap();
        let key = cx.string(column_name);
        let v: Handle<'_, JsValue> = match v {
            libsql::Value::Null => cx.null().upcast(),
            libsql::Value::Integer(v) => {
                if safe_ints {
                    neon::types::JsBigInt::from_i64(cx, v).upcast()
                } else {
                    cx.number(v as f64).upcast()
                }
            }
            libsql::Value::Real(v) => cx.number(v).upcast(),
            libsql::Value::Text(v) => cx.string(v).upcast(),
            libsql::Value::Blob(v) => JsArrayBuffer::from_slice(cx, &v)?.upcast(),
        };
        result.set(cx, key, v)?;
    }
    Ok(())
}

fn convert_row_raw(
    cx: &mut FunctionContext,
    safe_ints: bool,
    result: &mut JsArray,
    rows: &libsql::Rows,
    row: &libsql::Row,
) -> NeonResult<()> {
    for idx in 0..rows.column_count() {
        let v = row
            .get_value(idx)
            .or_else(|err| throw_libsql_error(cx, err))?;
        let v: Handle<'_, JsValue> = match v {
            libsql::Value::Null => cx.null().upcast(),
            libsql::Value::Integer(v) => {
                if safe_ints {
                    neon::types::JsBigInt::from_i64(cx, v).upcast()
                } else {
                    cx.number(v as f64).upcast()
                }
            }
            libsql::Value::Real(v) => cx.number(v).upcast(),
            libsql::Value::Text(v) => cx.string(v).upcast(),
            libsql::Value::Blob(v) => JsArrayBuffer::from_slice(cx, &v)?.upcast(),
        };
        result.set(cx, idx as u32, v)?;
    }
    Ok(())
}

#[neon::main]
fn main(mut cx: ModuleContext) -> NeonResult<()> {
    let _ = tracing_subscriber::fmt::try_init();
    cx.export_function("databaseOpen", Database::js_open)?;
    cx.export_function("databaseOpenWithRpcSync", Database::js_open_with_rpc_sync)?;
    cx.export_function("databaseInTransaction", Database::js_in_transaction)?;
    cx.export_function("databaseClose", Database::js_close)?;
    cx.export_function("databaseSyncSync", Database::js_sync_sync)?;
    cx.export_function("databaseSyncAsync", Database::js_sync_async)?;
    cx.export_function("databaseExecSync", Database::js_exec_sync)?;
    cx.export_function("databaseExecAsync", Database::js_exec_async)?;
    cx.export_function("databasePrepareSync", Database::js_prepare_sync)?;
    cx.export_function("databasePrepareAsync", Database::js_prepare_async)?;
    cx.export_function(
        "databaseDefaultSafeIntegers",
        Database::js_default_safe_integers,
    )?;
    cx.export_function("statementRaw", Statement::js_raw)?;
    cx.export_function("statementRun", Statement::js_run)?;
    cx.export_function("statementGet", Statement::js_get)?;
    cx.export_function("statementRowsSync", Statement::js_rows_sync)?;
    cx.export_function("statementRowsAsync", Statement::js_rows_async)?;
    cx.export_function("statementColumns", Statement::js_columns)?;
    cx.export_function("statementSafeIntegers", Statement::js_safe_integers)?;
    cx.export_function("rowsNext", Rows::js_next)?;
    Ok(())
}

fn version(protocol: &str) -> String {
    let ver = env!("CARGO_PKG_VERSION");
    format!("libsql-js-{protocol}-{ver}")
}
