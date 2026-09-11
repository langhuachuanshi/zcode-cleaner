#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

const PROTECT_MS: u128 = 24 * 3600 * 1000;

/// 会话磁盘数据位置（相对 ~/.zcode）。前四个取 sess_* 子目录，rollout 取 model-io-<id>.jsonl 单文件
const SESSION_DIR_LOCS: &[(&str, &str)] = &[
    ("agents", "cli/agents"),
    ("artifacts", "cli/artifacts"),
    ("exec", "cli/exec"),
    ("image-cache", "cli/image-cache"),
];
const ROLLOUT_DIR: &str = "cli/rollout";

/// 引用 session 的表和列（已实测探查；part 无外键约束但需要显式清理）
const SESSION_FK_COLS: &[(&str, &str)] = &[
    ("message", "session_id"),
    ("todo", "session_id"),
    ("session_entry", "session_id"),
    ("session_target", "session_id"),
    ("workflow_run", "parent_session_id"),
    ("workflow_activity", "child_session_id"),
    ("session_task_link", "child_session_id"),
    ("session_task_link", "parent_session_id"),
    ("model_usage", "session_id"),
    ("turn_usage", "session_id"),
    ("tool_usage", "session_id"),
    ("session_input", "session_id"),
    ("part", "session_id"),
];

// ---------- 基础工具 ----------

fn home() -> PathBuf {
    PathBuf::from(std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).expect("无法定位用户目录"))
}
fn zcode_root() -> PathBuf {
    std::env::var("ZCODE_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".zcode"))
}
fn db_path() -> PathBuf {
    zcode_root().join("cli/db/db.sqlite")
}
fn settings_path() -> PathBuf {
    home().join(".zcode-cleaner.json")
}
fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}
fn file_mtime_ms(p: &Path) -> u128 {
    fs::metadata(p).ok().and_then(|m| m.modified().ok()).map(|t| {
        t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
    }).unwrap_or(0)
}

/// 递归统计目录：返回 (字节数, 最新 mtime 毫秒)
fn dir_stats(p: &Path) -> (u64, u128) {
    let mut size = 0u64;
    let mut mtime = file_mtime_ms(p);
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            let ep = e.path();
            let md = match fs::symlink_metadata(&ep) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() {
                let (s, t) = dir_stats(&ep);
                size += s;
                if t > mtime {
                    mtime = t;
                }
            } else {
                size += md.len();
                let t = file_mtime_ms(&ep);
                if t > mtime {
                    mtime = t;
                }
            }
        }
    }
    (size, mtime)
}

fn open_db(read_only: bool) -> Result<Connection, String> {
    let p = db_path();
    if !p.exists() {
        return Err("数据库不存在".into());
    }
    let conn = if read_only {
        Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .or_else(|_| Connection::open(&p))
    } else {
        Connection::open(&p)
    }
    .map_err(|e| format!("打开数据库失败: {e}"))?;
    conn.busy_timeout(Duration::from_secs(5)).ok();
    Ok(conn)
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

// ---------- 数据结构 ----------

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SessionPart {
    loc: String,
    label: String,
    size: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionInfo {
    id: String,
    title: Option<String>,
    directory: Option<String>,
    time_updated: u128, // ms
    disk_size: u64,
    parts: Vec<SessionPart>,
    db_only: bool,   // 数据库有记录、磁盘无文件
    orphan: bool,    // 磁盘有文件、数据库无记录
    protected: bool, // 近 24h 活跃
    age_days: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LogFile {
    name: String,
    size: u64,
    mtime: u128,
    age_days: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CacheInfo {
    key: String,
    name: String,
    path: String,
    kind: String, // dated-log | tree
    note: String,
    total_size: u64,
    file_count: Option<usize>,
    files: Option<Vec<LogFile>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanReport {
    root: String,
    total_size: u64,
    db_size: u64,
    db_session_count: usize,
    db_error: Option<String>,
    db_busy_hint: bool, // zcode 可能正在运行（wal 最近有写入）
    sessions: Vec<SessionInfo>,
    caches: Vec<CacheInfo>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteReport {
    deleted: usize,
    skipped: Vec<String>,
    freed_disk: u64,
    db_rows_deleted: u64,
    errors: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CleanReport {
    deleted_files: usize,
    freed_disk: u64,
    errors: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    session_keep_days: u32,
    log_keep_days: u32,
    clean_plugin_cache: bool,
    clean_crash: bool,
    protect_active: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self { session_keep_days: 30, log_keep_days: 7, clean_plugin_cache: false, clean_crash: true, protect_active: true }
    }
}

// ---------- 进度通知 ----------

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Progress {
    stage: String,
    current: usize,
    total: usize,
    message: String,
}

fn notify(app: Option<&AppHandle>, stage: &str, current: usize, total: usize, msg: String) {
    if let Some(a) = app {
        let _ = a.emit("progress", Progress { stage: stage.into(), current, total, message: msg });
    }
}

// ---------- 扫描 ----------

/// 全盘总大小缓存：只在手动"重新扫描"时重算（递归全树较慢）
static TOTAL_CACHE: AtomicU64 = AtomicU64::new(0);
static TOTAL_VALID: AtomicBool = AtomicBool::new(false);

fn root_total(compute: bool) -> u64 {
    if compute || !TOTAL_VALID.load(Ordering::Relaxed) {
        let (t, _) = dir_stats(&zcode_root());
        TOTAL_CACHE.store(t, Ordering::Relaxed);
        TOTAL_VALID.store(true, Ordering::Relaxed);
        t
    } else {
        TOTAL_CACHE.load(Ordering::Relaxed)
    }
}

fn invalidate_total() {
    TOTAL_VALID.store(false, Ordering::Relaxed);
}

/// 文件系统侧：收集各位置 sess_* 的大小与时间。key: id -> (parts, size, mtime)
fn scan_fs_sessions() -> std::collections::HashMap<String, (Vec<SessionPart>, u64, u128)> {
    let mut map = std::collections::HashMap::new();
    let root = zcode_root();
    for (loc, dir) in SESSION_DIR_LOCS {
        let abs = root.join(dir);
        let Ok(rd) = fs::read_dir(&abs) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("sess_") || !e.path().is_dir() {
                continue;
            }
            let (size, mtime) = dir_stats(&e.path());
            let entry = map.entry(name.clone()).or_insert((Vec::new(), 0u64, 0u128));
            entry.0.push(SessionPart { loc: loc.to_string(), label: loc_label(loc), size });
            entry.1 += size;
            if mtime > entry.2 {
                entry.2 = mtime;
            }
        }
    }
    if let Ok(rd) = fs::read_dir(root.join(ROLLOUT_DIR)) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(id) = name.strip_prefix("model-io-").and_then(|s| s.strip_suffix(".jsonl")) else { continue };
            let size = fs::metadata(e.path()).map(|m| m.len()).unwrap_or(0);
            let mtime = file_mtime_ms(&e.path());
            let entry = map.entry(id.to_string()).or_insert((Vec::new(), 0u64, 0u128));
            entry.0.push(SessionPart { loc: "rollout".into(), label: "模型IO".into(), size });
            entry.1 += size;
            if mtime > entry.2 {
                entry.2 = mtime;
            }
        }
    }
    map
}

fn loc_label(loc: &str) -> String {
    match loc {
        "agents" => "转录".into(),
        "artifacts" => "产物".into(),
        "exec" => "Shell快照".into(),
        "image-cache" => "图片缓存".into(),
        "rollout" => "模型IO".into(),
        other => other.into(),
    }
}

fn scan_dated_log(rel: &str) -> CacheInfo {
    let abs = zcode_root().join(rel);
    let mut files = Vec::new();
    if let Ok(rd) = fs::read_dir(&abs) {
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if !md.is_file() {
                continue;
            }
            let mtime = file_mtime_ms(&e.path());
            files.push(LogFile {
                name: e.file_name().to_string_lossy().to_string(),
                size: md.len(),
                mtime,
                age_days: ((now_ms() - mtime) as f64 / 86400000.0).max(0.0),
            });
        }
    }
    files.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    let total_size = files.iter().map(|f| f.size).sum();
    CacheInfo {
        key: if rel.contains("cli/log") { "cli-log".into() } else { "v2-log".into() },
        name: if rel.contains("cli/log") { "CLI 日志".into() } else { "v2 日志".into() },
        path: rel.into(),
        kind: "dated-log".into(),
        note: "按日期的运行日志，可按保留天数清理".into(),
        total_size,
        file_count: Some(files.len()),
        files: Some(files),
    }
}

fn scan_tree(key: &str, name: &str, rel: &str, note: &str) -> CacheInfo {
    let (size, _) = dir_stats(&zcode_root().join(rel));
    CacheInfo {
        key: key.into(),
        name: name.into(),
        path: rel.into(),
        kind: "tree".into(),
        note: note.into(),
        total_size: size,
        file_count: None,
        files: None,
    }
}

#[tauri::command]
async fn scan(app: AppHandle, compute_total: Option<bool>) -> Result<ScanReport, String> {
    tauri::async_runtime::spawn_blocking(move || scan_impl(compute_total.unwrap_or(false), Some(&app)))
        .await
        .map_err(|e| format!("后台任务失败: {e}"))?
}

fn scan_impl(compute_total: bool, app: Option<&AppHandle>) -> Result<ScanReport, String> {
    notify(app, "scan", 0, 0, "正在扫描会话文件…".into());
    let now = now_ms();
    let fs_map = scan_fs_sessions();

    // 数据库侧
    notify(app, "scan", 1, 3, "正在读取会话数据库…".into());
    let mut sessions: Vec<SessionInfo> = Vec::new();
    let mut db_session_count = 0usize;
    let mut db_error = None;
    match open_db(true) {
        Ok(conn) => {
            let r = (|| -> rusqlite::Result<()> {
                let mut stmt = conn.prepare("SELECT id, title, directory, time_created, time_updated FROM session")?;
                let mut it = stmt.query([])?;
                while let Some(row) = it.next()? {
                    let id: String = row.get(0)?;
                    let title: Option<String> = row.get(1)?;
                    let directory: Option<String> = row.get(2)?;
                    let t_created: Option<i64> = row.get(3)?;
                    let t_updated: Option<i64> = row.get(4)?;
                    // 时间戳兼容秒/毫秒
                    let to_ms = |v: Option<i64>| match v {
                        Some(v) if v < 10_000_000_000 => (v as u128) * 1000,
                        Some(v) => v as u128,
                        None => 0,
                    };
                    let t = to_ms(t_updated).max(to_ms(t_created));
                    db_session_count += 1;
                    let (parts, disk_size, fs_mtime) = fs_map.get(&id).cloned().unwrap_or((Vec::new(), 0, 0));
                    let mtime = t.max(fs_mtime);
                    sessions.push(SessionInfo {
                        id,
                        title,
                        directory,
                        time_updated: mtime,
                        disk_size,
                        parts,
                        db_only: disk_size == 0,
                        orphan: false,
                        protected: now.saturating_sub(mtime) < PROTECT_MS,
                        age_days: (now.saturating_sub(mtime) as f64 / 86400000.0).max(0.0),
                    });
                }
                Ok(())
            })();
            if let Err(e) = r {
                db_error = Some(format!("读取会话表失败: {e}"));
            }
        }
        Err(e) => db_error = Some(e),
    }

    // 磁盘有而数据库无的（孤儿）
    for (id, (parts, size, mtime)) in fs_map {
        if !sessions.iter().any(|s| s.id == id) {
            sessions.push(SessionInfo {
                id,
                title: None,
                directory: None,
                time_updated: mtime,
                disk_size: size,
                parts,
                db_only: false,
                orphan: true,
                protected: now.saturating_sub(mtime) < PROTECT_MS,
                age_days: (now.saturating_sub(mtime) as f64 / 86400000.0).max(0.0),
            });
        }
    }
    sessions.sort_by(|a, b| b.time_updated.cmp(&a.time_updated));

    notify(app, "scan", 2, 3, "正在扫描缓存…".into());
    let caches = vec![
        scan_dated_log("cli/log"),
        scan_dated_log("v2/logs"),
        scan_tree("plugin-cache", "插件缓存", "cli/plugins/cache", "插件下载缓存，清理后按需重新下载"),
        scan_tree("crash", "崩溃转储", "v2/crash", "崩溃 .dmp 诊断文件，通常无用"),
    ];

    let wal = db_path().with_extension("sqlite-wal");
    let db_busy_hint = wal.exists() && now.saturating_sub(file_mtime_ms(&wal)) < 120_000;
    if compute_total {
        notify(app, "scan", 3, 4, "正在统计总占用（较慢）…".into());
    }
    let db_size = fs::metadata(db_path()).map(|m| m.len()).unwrap_or(0);

    Ok(ScanReport {
        root: zcode_root().to_string_lossy().to_string(),
        total_size: root_total(compute_total),
        db_size,
        db_session_count,
        db_error,
        db_busy_hint,
        sessions,
        caches,
    })
}

// ---------- 删除会话（文件 + 数据库索引/历史） ----------

fn valid_sess_id(id: &str) -> bool {
    id.starts_with("sess_")
        && id.len() > 5
        && id[5..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[tauri::command]
async fn delete_sessions(app: AppHandle, ids: Vec<String>, force: bool) -> Result<DeleteReport, String> {
    tauri::async_runtime::spawn_blocking(move || delete_sessions_impl(ids, force, Some(&app)))
        .await
        .map_err(|e| format!("后台任务失败: {e}"))?
}

fn delete_sessions_impl(ids: Vec<String>, force: bool, app: Option<&AppHandle>) -> Result<DeleteReport, String> {
    let now = now_ms();
    let mut report = DeleteReport { deleted: 0, skipped: vec![], freed_disk: 0, db_rows_deleted: 0, errors: vec![] };
    let root = zcode_root();

    let conn = open_db(false)?;
    let ro_conn = open_db(true).ok();

    let total = ids.len();
    for (idx, id) in ids.iter().enumerate() {
        notify(app, "delete", idx, total, format!("正在删除会话 {}/{}：{}", idx + 1, total, id.chars().take(24).collect::<String>()));
        if !valid_sess_id(id) {
            report.skipped.push(format!("{id}: 非法 ID"));
            continue;
        }
        // 保护检查：取磁盘与数据库时间的较大者
        let mut mtime = 0u128;
        for (_, dir) in SESSION_DIR_LOCS {
            let t = dir_stats(&root.join(dir).join(id)).1;
            if t > mtime {
                mtime = t;
            }
        }
        let t = file_mtime_ms(&root.join(ROLLOUT_DIR).join(format!("model-io-{id}.jsonl")));
        if t > mtime {
            mtime = t;
        }
        if let Some(c) = &ro_conn {
            if let Ok(v) = c.query_row("SELECT COALESCE(MAX(time_updated),0) FROM session WHERE id=?1", [id], |r| r.get::<_, Option<i64>>(0)) {
                let v = match v { Some(v) if (v as u128) < 10_000_000_000 => (v as u128) * 1000, Some(v) => v as u128, None => 0 };
                if v > mtime {
                    mtime = v;
                }
            }
        }
        if !force && now.saturating_sub(mtime) < PROTECT_MS {
            report.skipped.push(format!("{id}: 近 24h 活跃，处于保护中"));
            continue;
        }

        // 1) 删数据库行（本会话 + 递归子会话，含全部关联表），拿到整棵树
        let tree = match delete_db_rows(&conn, id) {
            Ok((n, tree)) => {
                report.db_rows_deleted += n;
                tree
            }
            Err(e) => {
                report.errors.push(format!("{id}/db: {e}"));
                vec![id.clone()]
            }
        };

        // 2) 删整棵树（含 subagent 子会话）的磁盘文件
        let mut freed = 0u64;
        for sid in &tree {
            for (loc, dir) in SESSION_DIR_LOCS {
                let p = root.join(dir).join(sid);
                if p.exists() {
                    freed += dir_stats(&p).0;
                    if let Err(e) = fs::remove_dir_all(&p) {
                        report.errors.push(format!("{sid}/{loc}: {e}"));
                    }
                }
            }
            let rp = root.join(ROLLOUT_DIR).join(format!("model-io-{sid}.jsonl"));
            if rp.exists() {
                freed += fs::metadata(&rp).map(|m| m.len()).unwrap_or(0);
                if let Err(e) = fs::remove_file(&rp) {
                    report.errors.push(format!("{sid}/rollout: {e}"));
                }
            }
        }
        report.freed_disk += freed;
        report.deleted += 1;
    }
    invalidate_total();
    Ok(report)
}

/// 删除会话树在数据库中的全部行，返回 (删除行数, 整棵树的 ID 列表)
fn delete_db_rows(conn: &Connection, id: &str) -> Result<(u64, Vec<String>), String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let mut all_ids = vec![id.to_string()];
    // 递归收集子会话（subagent）
    loop {
        let sql = format!(
            "SELECT DISTINCT id FROM session WHERE parent_id IN ({ph}) AND id NOT IN ({ph})",
            ph = placeholders(all_ids.len())
        );
        let mut stmt = tx.prepare(&sql).map_err(|e| e.to_string())?;
        let params: Vec<&dyn rusqlite::ToSql> = all_ids.iter().map(|s| s as &dyn rusqlite::ToSql)
            .chain(all_ids.iter().map(|s| s as &dyn rusqlite::ToSql)).collect();
        let kids: Vec<String> = stmt.query_map(params.as_slice(), |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .flatten().collect();
        if kids.is_empty() { break; }
        for k in kids {
            if !all_ids.contains(&k) { all_ids.push(k); }
        }
    }
    let n = all_ids.len();
    let ph = placeholders(n);
    let mut rows = 0u64;
    for (tbl, col) in SESSION_FK_COLS {
        let sql = format!("DELETE FROM {tbl} WHERE {col} IN ({ph})");
        rows += tx.execute(&sql, rusqlite::params_from_iter(all_ids.iter())).map_err(|e| format!("{tbl}: {e}"))? as u64;
    }
    rows += tx.execute(&format!("DELETE FROM session WHERE id IN ({ph})"), rusqlite::params_from_iter(all_ids.iter()))
        .map_err(|e| e.to_string())? as u64;
    tx.commit().map_err(|e| format!("提交事务失败: {e}"))?;
    Ok((rows, all_ids))
}

// ---------- 清理缓存（永久删除） ----------

#[tauri::command]
async fn clean_caches(app: AppHandle, keys: Vec<String>, keep_days: u32) -> Result<CleanReport, String> {
    tauri::async_runtime::spawn_blocking(move || clean_caches_impl(keys, keep_days, Some(&app)))
        .await
        .map_err(|e| format!("后台任务失败: {e}"))?
}

fn clean_caches_impl(keys: Vec<String>, keep_days: u32, app: Option<&AppHandle>) -> Result<CleanReport, String> {
    let mut report = CleanReport { deleted_files: 0, freed_disk: 0, errors: vec![] };
    let cutoff = now_ms().saturating_sub(keep_days as u128 * 86_400_000);
    let total = keys.len();
    for (idx, key) in keys.iter().enumerate() {
        notify(app, "clean", idx, total, format!("正在清理缓存 {}/{}：{}", idx + 1, total, key));
        match key.as_str() {
            "cli-log" => clean_dated_log("cli/log", cutoff, &mut report),
            "v2-log" => clean_dated_log("v2/logs", cutoff, &mut report),
            "plugin-cache" => clean_tree("cli/plugins/cache", &mut report),
            "crash" => clean_tree("v2/crash", &mut report),
            other => report.errors.push(format!("{other}: 未知类别")),
        }
    }
    invalidate_total();
    Ok(report)
}

fn clean_dated_log(rel: &str, cutoff: u128, report: &mut CleanReport) {
    let dir = zcode_root().join(rel);
    let Ok(rd) = fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if !md.is_file() || file_mtime_ms(&e.path()) > cutoff {
            continue;
        }
        report.freed_disk += md.len();
        if let Err(err) = fs::remove_file(e.path()) {
            report.errors.push(format!("{rel}/{}: {err}", e.file_name().to_string_lossy()));
        } else {
            report.deleted_files += 1;
        }
    }
}

fn clean_tree(rel: &str, report: &mut CleanReport) {
    let p = zcode_root().join(rel);
    if !p.exists() {
        return;
    }
    let (size, _) = dir_stats(&p);
    report.freed_disk += size;
    if let Err(err) = fs::remove_dir_all(&p) {
        report.errors.push(format!("{rel}: {err}"));
    } else {
        let _ = fs::create_dir_all(&p); // 留空目录，zcode 按需重建
    }
}

// ---------- 数据库瘦身 ----------

#[tauri::command]
async fn vacuum_db(app: AppHandle) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || vacuum_impl(Some(&app)))
        .await
        .map_err(|e| format!("后台任务失败: {e}"))?
}

fn vacuum_impl(app: Option<&AppHandle>) -> Result<serde_json::Value, String> {
    let p = db_path();
    let before = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    notify(app, "vacuum", 0, 2, "正在合并 WAL 日志…".into());
    let conn = open_db(false)?;
    if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
        return Err(format!("checkpoint 失败（ZCode 可能正在运行，请先退出）: {e}"));
    }
    notify(app, "vacuum", 1, 2, "正在压缩数据库（可能需要几十秒）…".into());
    conn.execute_batch("VACUUM;").map_err(|e| format!("VACUUM 失败（建议退出 ZCode 后重试）: {e}"))?;
    let after = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    Ok(serde_json::json!({ "before": before, "after": after }))
}

// ---------- 设置持久化 ----------

#[tauri::command]
fn load_settings() -> Settings {
    fs::read_to_string(settings_path()).ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn save_settings(settings: Settings) -> Result<(), String> {
    let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    fs::write(settings_path(), json).map_err(|e| e.to_string())
}

/// 命令行模式（不开窗口）：--scan | --delete-sessions <id,...> [--force] | --vacuum
fn cli_run(args: &[String]) {
    let json = |v: serde_json::Value| println!("{v}");
    let r = match args[1].as_str() {
        "--scan" => scan_impl(true, None).map(|r| serde_json::to_value(r).unwrap()),
        "--delete-sessions" => {
            if args.len() < 3 {
                Err("用法: zcode-cleaner --delete-sessions <sess_id[,sess_id2...]> [--force]".into())
            } else {
                let ids: Vec<String> = args[2].split(',').map(|s| s.trim().to_string()).collect();
                let force = args.iter().any(|a| a == "--force");
                delete_sessions_impl(ids, force, None).map(|r| serde_json::to_value(r).unwrap())
            }
        }
        "--vacuum" => vacuum_impl(None),
        other => Err(format!("未知参数 {other}。用法: --scan | --delete-sessions <id,...> [--force] | --vacuum")),
    };
    match r {
        Ok(v) => json(v),
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::exit(1);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        cli_run(&args);
        return;
    }
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            scan,
            delete_sessions,
            clean_caches,
            vacuum_db,
            load_settings,
            save_settings
        ])
        .run(tauri::generate_context!())
        .expect("tauri 启动失败");
}
