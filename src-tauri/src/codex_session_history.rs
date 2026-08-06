use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

use crate::{
    codex_environment::default_codex_home,
    database::Repository,
    domain::{
        CodexHistoryExportReport, CodexHistoryHomeSummary, CodexHistoryImportReport,
        CodexHistoryMutationReport, CodexHistoryProjectSummary, CodexHistoryReport,
        CodexHistorySessionSummary, CodexHistorySourceSummary, CodexHistorySyncReport,
        DeleteCodexHistoryInput, ExportCodexHistoryInput, ImportCodexHistoryInput,
        ListCodexHistoryInput,
    },
    error::{AppError, AppResult},
};

const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(6);
const DEFAULT_MODEL_PROVIDER: &str = "openai";
const DEFAULT_HISTORY_LIST_LIMIT: usize = 100;
const MAX_HISTORY_LIST_LIMIT: usize = 500;
const UNCATEGORIZED_PROJECT_ID: &str = "__uncategorized__";
const UNCATEGORIZED_PROJECT_NAME: &str = "未归类";
const HISTORY_EXPORT_FORMAT: &str = "codex-relay-history-v1";

#[derive(Debug, Clone)]
struct CodexHome {
    id: String,
    kind: String,
    label: String,
    path: PathBuf,
    sync_target: bool,
}

#[derive(Debug, Clone)]
struct RolloutSnapshot {
    session_id: String,
    title: Option<String>,
    cwd: Option<String>,
    updated_at_ms: i64,
    rollout_path: PathBuf,
    relative_path: PathBuf,
    home: CodexHome,
    source_rank: usize,
    lines: Vec<RolloutLine>,
}

#[derive(Debug, Clone)]
struct HistoryListSnapshot {
    session_id: String,
    title: Option<String>,
    cwd: Option<String>,
    updated_at_ms: i64,
    event_count: usize,
    sha256: String,
    normalized_sha256: String,
    rollout_path: PathBuf,
    archived: bool,
    home: CodexHome,
    source_rank: usize,
}

#[derive(Debug, Clone)]
struct RolloutLine {
    raw: String,
    value: Option<Value>,
    timestamp_ms: i64,
    source_rank: usize,
    line_index: usize,
}

#[derive(Debug, Clone)]
struct IndexEntry {
    title: Option<String>,
    updated_at_ms: Option<i64>,
}

#[derive(Debug, Clone)]
struct SqliteThreadEntry {
    title: Option<String>,
    cwd: Option<String>,
    updated_at_ms: Option<i64>,
    archived: Option<bool>,
}

#[derive(Debug, Clone)]
struct ProjectIdentity {
    id: String,
    name: String,
    cwd: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct HistoryExportManifest {
    format: String,
    exported_at_ms: i64,
    sessions: Vec<HistoryExportManifestSession>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct HistoryExportManifestSession {
    id: String,
    title: Option<String>,
    cwd: Option<String>,
    project_id: String,
    project_name: String,
    updated_at_ms: i64,
    rollout_path: String,
}

#[derive(Debug, Clone)]
struct HistoryListPage {
    limit: usize,
    offset: usize,
    project_id: Option<String>,
}

#[derive(Default)]
struct SyncStats {
    files_written: usize,
    files_backed_up: usize,
    sessions_synced: usize,
    changed_home_ids: HashSet<String>,
}

#[derive(Default)]
struct ScanResult {
    homes: Vec<CodexHome>,
    snapshots: Vec<RolloutSnapshot>,
    warnings: Vec<String>,
}

#[derive(Default)]
struct ListScanResult {
    homes: Vec<CodexHome>,
    snapshots: Vec<HistoryListSnapshot>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct ExtraTargetHome {
    id: String,
    kind: String,
    label: String,
    path: PathBuf,
    sync_target: bool,
}

pub fn list_codex_history(
    repository: &Repository,
    data_dir: &Path,
    input: ListCodexHistoryInput,
) -> AppResult<CodexHistoryReport> {
    let page = history_list_page(input);
    let scan = scan_history_for_list(repository, data_dir)?;
    Ok(report_from_list_scan(scan, page))
}

pub fn sync_codex_history(
    repository: &Repository,
    data_dir: &Path,
) -> AppResult<CodexHistorySyncReport> {
    sync_codex_history_with_options(repository, data_dir, None, None, None)
}

pub fn sync_codex_history_prioritized_home(
    repository: &Repository,
    data_dir: &Path,
    priority_home: Option<&Path>,
) -> AppResult<CodexHistorySyncReport> {
    sync_codex_history_with_options(repository, data_dir, None, None, priority_home)
}

pub fn restore_codex_session_to_home(
    repository: &Repository,
    data_dir: &Path,
    session_id: &str,
    target_home: &Path,
    target_label: &str,
) -> AppResult<CodexHistorySyncReport> {
    sync_codex_history_with_options(
        repository,
        data_dir,
        Some(ExtraTargetHome {
            id: format!("restore:{}", stable_path_id(target_home)),
            kind: "collaboration_restore".to_owned(),
            label: target_label.to_owned(),
            path: target_home.to_path_buf(),
            sync_target: true,
        }),
        Some(&HashSet::from([session_id.to_owned()])),
        None,
    )
}

pub fn delete_codex_history(
    repository: &Repository,
    data_dir: &Path,
    input: DeleteCodexHistoryInput,
) -> AppResult<CodexHistoryMutationReport> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let mut scan = scan_history_for_list(repository, data_dir)?;
    let scanned_at_ms = timestamp_ms();
    let session_ids = selected_list_session_ids(
        &scan.snapshots,
        &input.scope,
        &input.session_ids,
        input.project_id.as_deref(),
    )?;
    if session_ids.is_empty() {
        return Err(AppError::NotFound);
    }
    let trash_root = data_dir
        .join("session-history-trash")
        .join(format!("{}", scanned_at_ms));
    let mut stats = SyncStats::default();
    let mut files_removed = 0;
    let mut warnings = std::mem::take(&mut scan.warnings);
    let target_homes = scan.homes.clone();
    for home in &target_homes {
        let home_snapshots = scan
            .snapshots
            .iter()
            .filter(|snapshot| {
                snapshot.home.id == home.id && session_ids.contains(&snapshot.session_id)
            })
            .collect::<Vec<_>>();
        for snapshot in home_snapshots {
            backup_file(&snapshot.rollout_path, &trash_root, home, &mut stats)?;
            fs::remove_file(&snapshot.rollout_path).map_err(|_| AppError::RuntimeUnavailable)?;
            files_removed += 1;
            stats.changed_home_ids.insert(home.id.clone());
        }
        remove_empty_session_dirs(&home.path);
        if remove_sessions_from_index(home, &session_ids, &trash_root, &mut stats, &mut warnings)? {
            stats.changed_home_ids.insert(home.id.clone());
        }
        if delete_sqlite_threads(home, &session_ids, &trash_root, &mut stats, &mut warnings)? {
            stats.changed_home_ids.insert(home.id.clone());
        }
    }
    let metadata_rebuilt = rebuild_changed_home_metadata(&target_homes, &stats, &mut warnings);
    let status = if warnings.is_empty() {
        "completed"
    } else {
        "warning"
    };
    Ok(CodexHistoryMutationReport {
        status: status.to_owned(),
        message: format!(
            "已将 {} 个 Codex 会话移入 Relay 回收站。",
            session_ids.len()
        ),
        scanned_at_ms,
        sessions_affected: session_ids.len(),
        files_removed,
        files_backed_up: stats.files_backed_up,
        metadata_updated: stats.files_written,
        metadata_rebuilt,
        warnings,
    })
}

pub fn export_codex_history(
    repository: &Repository,
    data_dir: &Path,
    input: ExportCodexHistoryInput,
) -> AppResult<CodexHistoryExportReport> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    let mut scan = scan_history(repository, data_dir, None)?;
    let scanned_at_ms = timestamp_ms();
    let destination = PathBuf::from(input.destination_path.trim());
    if destination.as_os_str().is_empty() {
        return Err(AppError::ValidationFailed);
    }
    let groups = snapshots_by_session(&scan.snapshots);
    let selected = selected_rollout_groups(
        groups,
        &input.scope,
        &input.session_ids,
        input.project_id.as_deref(),
    )?;
    if selected.is_empty() {
        return Err(AppError::NotFound);
    }
    if let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
    }
    let mut manifest_sessions = Vec::new();
    let mut rendered_rollouts = Vec::<(String, String)>::new();
    for (session_id, snapshots) in selected {
        let canonical = canonical_rollout(&session_id, &snapshots);
        let project = project_identity(canonical.cwd.as_deref());
        let rollout_path = format!("rollouts/{session_id}.jsonl");
        manifest_sessions.push(HistoryExportManifestSession {
            id: session_id.clone(),
            title: canonical.title.clone(),
            cwd: canonical.cwd.clone(),
            project_id: project.id,
            project_name: project.name,
            updated_at_ms: canonical.updated_at_ms,
            rollout_path: rollout_path.clone(),
        });
        rendered_rollouts.push((rollout_path, render_rollout_lines_raw(&canonical.lines)));
    }
    manifest_sessions.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    rendered_rollouts.sort_by(|left, right| left.0.cmp(&right.0));
    let manifest = HistoryExportManifest {
        format: HISTORY_EXPORT_FORMAT.to_owned(),
        exported_at_ms: scanned_at_ms,
        sessions: manifest_sessions,
    };
    write_history_export_zip(&destination, &manifest, &rendered_rollouts)?;
    let status = if scan.warnings.is_empty() {
        "completed"
    } else {
        "warning"
    };
    Ok(CodexHistoryExportReport {
        status: status.to_owned(),
        message: format!("已导出 {} 个 Codex 会话。", rendered_rollouts.len()),
        scanned_at_ms,
        sessions_exported: rendered_rollouts.len(),
        files_exported: rendered_rollouts.len() + 1,
        destination_path: destination.display().to_string(),
        warnings: std::mem::take(&mut scan.warnings),
    })
}

pub fn import_codex_history(
    repository: &Repository,
    data_dir: &Path,
    input: ImportCodexHistoryInput,
) -> AppResult<CodexHistoryImportReport> {
    if !input.confirmed {
        return Err(AppError::ConfirmationRequired);
    }
    if input.paths.is_empty() {
        return Err(AppError::ValidationFailed);
    }
    let scanned_at_ms = timestamp_ms();
    let import_home = data_dir
        .join("session-history-imports")
        .join(format!("{}", scanned_at_ms))
        .join("codex-home");
    fs::create_dir_all(import_home.join("sessions/imported"))
        .map_err(|_| AppError::RuntimeUnavailable)?;
    let mut warnings = Vec::new();
    for path in &input.paths {
        import_history_path(Path::new(path), &import_home, &mut warnings)?;
    }
    let imported_session_ids = imported_session_ids(&import_home, &mut warnings)?;
    if imported_session_ids.is_empty() {
        return Err(AppError::NotFound);
    }
    let sync_report = sync_codex_history_with_options(
        repository,
        data_dir,
        Some(ExtraTargetHome {
            id: format!("import:{}", stable_path_id(&import_home)),
            kind: "history_import".to_owned(),
            label: "导入历史".to_owned(),
            path: import_home.clone(),
            sync_target: false,
        }),
        Some(&imported_session_ids),
        None,
    )?;
    warnings.extend(sync_report.warnings);
    if let Err(error) = fs::remove_dir_all(import_home.parent().unwrap_or(&import_home)) {
        warnings.push(format!("导入临时目录清理未完成：{error}"));
    }
    let status = if warnings.is_empty() {
        "completed"
    } else {
        "warning"
    };
    Ok(CodexHistoryImportReport {
        status: status.to_owned(),
        message: format!("已导入 {} 个 Codex 会话。", imported_session_ids.len()),
        scanned_at_ms,
        sessions_imported: imported_session_ids.len(),
        files_written: sync_report.files_written,
        files_backed_up: sync_report.files_backed_up,
        metadata_rebuilt: sync_report.metadata_rebuilt,
        warnings,
    })
}

fn sync_codex_history_with_options(
    repository: &Repository,
    data_dir: &Path,
    extra_target: Option<ExtraTargetHome>,
    only_session_ids: Option<&HashSet<String>>,
    priority_home: Option<&Path>,
) -> AppResult<CodexHistorySyncReport> {
    let mut scan = scan_history(repository, data_dir, extra_target)?;
    let scanned_at_ms = timestamp_ms();
    let backup_root = data_dir
        .join("session-history-backups")
        .join(format!("{}", scanned_at_ms));
    let mut target_homes = scan
        .homes
        .iter()
        .filter(|home| home.sync_target)
        .cloned()
        .collect::<Vec<_>>();
    prioritize_target_homes(&mut target_homes, priority_home);
    let mut groups = snapshots_by_session(&scan.snapshots);
    if let Some(session_ids) = only_session_ids {
        groups.retain(|id, _| session_ids.contains(id));
        if groups.is_empty() {
            return Err(AppError::NotFound);
        }
    }

    let mut stats = SyncStats::default();
    let mut cwd_by_home = HashMap::<String, Vec<String>>::new();
    let mut warnings = std::mem::take(&mut scan.warnings);

    for (session_id, snapshots) in groups {
        let canonical = canonical_rollout(&session_id, &snapshots);
        let mut session_changed = false;
        for home in &target_homes {
            let provider = model_provider_for_home(&home.path);
            let target_path = target_path_for_session(home, &snapshots, &canonical.primary);
            let rendered = render_rollout_lines(&canonical.lines, &provider);
            if write_if_changed(
                &target_path,
                rendered.as_bytes(),
                &backup_root,
                home,
                &mut stats,
            )? {
                session_changed = true;
            }
            if upsert_session_index(
                home,
                &canonical,
                &target_path,
                &backup_root,
                &mut stats,
                &mut warnings,
            )? {
                session_changed = true;
            }
            if repair_sqlite_thread(
                home,
                &canonical,
                &target_path,
                &provider,
                &backup_root,
                &mut stats,
                &mut warnings,
            )? {
                session_changed = true;
            }
            if let Some(cwd) = canonical
                .cwd
                .as_deref()
                .filter(|cwd| !cwd.trim().is_empty())
            {
                cwd_by_home
                    .entry(home.id.clone())
                    .or_default()
                    .push(cwd.to_owned());
            }
        }
        if session_changed {
            stats.sessions_synced += 1;
        }
    }

    for home in &target_homes {
        let cwds = cwd_by_home.remove(&home.id).unwrap_or_default();
        if update_global_state(home, &cwds, &backup_root, &mut stats, &mut warnings)? {
            stats.changed_home_ids.insert(home.id.clone());
        }
    }

    let mut metadata_rebuilt = 0;
    for home in &target_homes {
        if !stats.changed_home_ids.contains(&home.id) {
            continue;
        }
        match rebuild_thread_metadata(&home.path) {
            Ok(()) => metadata_rebuilt += 1,
            Err(error) => {
                warnings.push(format!("{} metadata rebuild 未完成：{}", home.label, error))
            }
        }
    }

    let sessions_seen = if let Some(session_ids) = only_session_ids {
        stats.sessions_synced.max(session_ids.len())
    } else {
        snapshots_by_session(&scan.snapshots).len()
    };
    let status = if warnings.is_empty() {
        "completed"
    } else {
        "warning"
    };
    let message = if stats.files_written == 0 {
        "Codex 会话历史已检查，未发现需要同步的差异。".to_owned()
    } else {
        format!(
            "Codex 会话历史已同步：{} 个会话，{} 个文件/元数据项更新。",
            stats.sessions_synced, stats.files_written
        )
    };
    Ok(CodexHistorySyncReport {
        status: status.to_owned(),
        message,
        scanned_at_ms,
        homes_scanned: scan.homes.len(),
        sessions_seen,
        sessions_synced: stats.sessions_synced,
        files_written: stats.files_written,
        files_backed_up: stats.files_backed_up,
        metadata_rebuilt,
        warnings,
    })
}

fn scan_history(
    repository: &Repository,
    data_dir: &Path,
    extra_target: Option<ExtraTargetHome>,
) -> AppResult<ScanResult> {
    let homes = collect_homes(repository, data_dir, extra_target)?;
    let mut snapshots = Vec::new();
    let mut warnings = Vec::new();
    for (rank, home) in homes.iter().enumerate() {
        let index = read_session_index(&home.path, &mut warnings);
        for path in rollout_paths(&home.path, &mut warnings) {
            match read_rollout_snapshot(home, rank, &path, &index) {
                Ok(Some(snapshot)) => snapshots.push(snapshot),
                Ok(None) => {}
                Err(error) => warnings.push(format!("{} 读取失败：{}", path.display(), error)),
            }
        }
    }
    Ok(ScanResult {
        homes,
        snapshots,
        warnings,
    })
}

fn scan_history_for_list(repository: &Repository, data_dir: &Path) -> AppResult<ListScanResult> {
    let homes = collect_homes(repository, data_dir, None)?;
    let mut snapshots = Vec::new();
    let mut warnings = Vec::new();
    for (rank, home) in homes.iter().enumerate() {
        let index = read_session_index(&home.path, &mut warnings);
        let sqlite = read_sqlite_thread_index(&home.path, &mut warnings);
        for path in rollout_paths(&home.path, &mut warnings) {
            match read_rollout_list_snapshot(home, rank, &path, &index, &sqlite) {
                Ok(Some(snapshot)) => snapshots.push(snapshot),
                Ok(None) => {}
                Err(error) => warnings.push(format!("{} 读取失败：{}", path.display(), error)),
            }
        }
    }
    Ok(ListScanResult {
        homes,
        snapshots,
        warnings,
    })
}

fn history_list_page(input: ListCodexHistoryInput) -> HistoryListPage {
    HistoryListPage {
        limit: input
            .limit
            .unwrap_or(DEFAULT_HISTORY_LIST_LIMIT)
            .clamp(1, MAX_HISTORY_LIST_LIMIT),
        offset: input.offset.unwrap_or_default(),
        project_id: input.project_id.filter(|id| !id.trim().is_empty()),
    }
}

fn selected_list_session_ids(
    snapshots: &[HistoryListSnapshot],
    scope: &str,
    session_ids: &[String],
    project_id: Option<&str>,
) -> AppResult<HashSet<String>> {
    match scope {
        "sessions" => {
            let requested = session_ids
                .iter()
                .map(|id| id.trim())
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect::<HashSet<_>>();
            if requested.is_empty() {
                return Err(AppError::ValidationFailed);
            }
            let available = snapshots
                .iter()
                .map(|snapshot| snapshot.session_id.clone())
                .collect::<HashSet<_>>();
            Ok(requested.intersection(&available).cloned().collect())
        }
        "project" => {
            let Some(project_id) = project_id else {
                return Err(AppError::ValidationFailed);
            };
            Ok(snapshots
                .iter()
                .filter(|snapshot| project_identity(snapshot.cwd.as_deref()).id == project_id)
                .map(|snapshot| snapshot.session_id.clone())
                .collect())
        }
        _ => Err(AppError::ValidationFailed),
    }
}

fn selected_rollout_groups(
    groups: HashMap<String, Vec<RolloutSnapshot>>,
    scope: &str,
    session_ids: &[String],
    project_id: Option<&str>,
) -> AppResult<HashMap<String, Vec<RolloutSnapshot>>> {
    match scope {
        "all" => Ok(groups),
        "sessions" => {
            let requested = session_ids
                .iter()
                .map(|id| id.trim())
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect::<HashSet<_>>();
            if requested.is_empty() {
                return Err(AppError::ValidationFailed);
            }
            Ok(groups
                .into_iter()
                .filter(|(session_id, _)| requested.contains(session_id))
                .collect())
        }
        "project" => {
            let Some(project_id) = project_id else {
                return Err(AppError::ValidationFailed);
            };
            Ok(groups
                .into_iter()
                .filter(|(_, snapshots)| {
                    project_identity(primary_snapshot(snapshots).cwd.as_deref()).id == project_id
                })
                .collect())
        }
        _ => Err(AppError::ValidationFailed),
    }
}

fn remove_sessions_from_index(
    home: &CodexHome,
    session_ids: &HashSet<String>,
    backup_root: &Path,
    stats: &mut SyncStats,
    warnings: &mut Vec<String>,
) -> AppResult<bool> {
    let path = home.path.join("session_index.jsonl");
    let Ok(current) = fs::read_to_string(&path) else {
        return Ok(false);
    };
    let mut changed = false;
    let mut lines = Vec::new();
    for (line_number, line) in current.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => {
                if string_field(&value, &["id", "session_id", "conversation_id"])
                    .as_ref()
                    .is_some_and(|id| session_ids.contains(id))
                {
                    changed = true;
                } else {
                    lines.push(line.to_owned());
                }
            }
            Err(_) => {
                warnings.push(format!(
                    "{}:{} index 行不是有效 JSON，删除时已原样保留。",
                    path.display(),
                    line_number + 1
                ));
                lines.push(line.to_owned());
            }
        }
    }
    if !changed {
        return Ok(false);
    }
    let next = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    write_if_changed(&path, next.as_bytes(), backup_root, home, stats)
}

fn delete_sqlite_threads(
    home: &CodexHome,
    session_ids: &HashSet<String>,
    backup_root: &Path,
    stats: &mut SyncStats,
    warnings: &mut Vec<String>,
) -> AppResult<bool> {
    let path = home.path.join("state_5.sqlite");
    if !path.exists() {
        return Ok(false);
    }
    let mut connection = match Connection::open(&path) {
        Ok(connection) => connection,
        Err(_) => {
            warnings.push(format!(
                "{} sqlite 暂不可写，删除时已跳过。",
                path.display()
            ));
            return Ok(false);
        }
    };
    let Some(columns) = sqlite_thread_columns(&connection) else {
        warnings.push(format!(
            "{} threads 表暂不可读，删除时已跳过。",
            path.display()
        ));
        return Ok(false);
    };
    if !columns.contains("id") {
        return Ok(false);
    }
    let mut existing = 0;
    for session_id in session_ids {
        existing += connection
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = ?1",
                params![session_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or_default();
    }
    if existing == 0 {
        return Ok(false);
    }
    backup_file(&path, backup_root, home, stats)?;
    let transaction = connection
        .transaction()
        .map_err(|_| AppError::LocalStateUnavailable)?;
    for session_id in session_ids {
        transaction
            .execute("DELETE FROM threads WHERE id = ?1", params![session_id])
            .map_err(|_| AppError::LocalStateUnavailable)?;
    }
    transaction
        .commit()
        .map_err(|_| AppError::LocalStateUnavailable)?;
    stats.files_written += 1;
    Ok(true)
}

fn remove_empty_session_dirs(home: &Path) {
    for root_name in ["sessions", "archived_sessions"] {
        let root = home.join(root_name);
        let mut dirs = Vec::new();
        collect_dirs(&root, &mut dirs);
        dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for dir in dirs {
            let _ = fs::remove_dir(&dir);
        }
    }
}

fn collect_dirs(root: &Path, dirs: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    dirs.push(root.to_path_buf());
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_dirs(&path, dirs);
        }
    }
}

fn rebuild_changed_home_metadata(
    homes: &[CodexHome],
    stats: &SyncStats,
    warnings: &mut Vec<String>,
) -> usize {
    let mut metadata_rebuilt = 0;
    for home in homes {
        if !stats.changed_home_ids.contains(&home.id) {
            continue;
        }
        match rebuild_thread_metadata(&home.path) {
            Ok(()) => metadata_rebuilt += 1,
            Err(error) => {
                warnings.push(format!("{} metadata rebuild 未完成：{}", home.label, error))
            }
        }
    }
    metadata_rebuilt
}

fn render_rollout_lines_raw(lines: &[RolloutLine]) -> String {
    let mut rendered = String::new();
    for line in lines {
        rendered.push_str(&line.raw);
        rendered.push('\n');
    }
    rendered
}

fn write_history_export_zip(
    destination: &Path,
    manifest: &HistoryExportManifest,
    rollouts: &[(String, String)],
) -> AppResult<()> {
    let temporary = destination.with_file_name(format!(
        ".{}.history-export-{}.tmp",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("codex-history-export"),
        Uuid::new_v4()
    ));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| AppError::RuntimeUnavailable)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("manifest.json", options)
        .map_err(|_| AppError::RuntimeUnavailable)?;
    let manifest_bytes = serde_json::to_vec_pretty(manifest).map_err(|_| AppError::Internal)?;
    zip.write_all(&manifest_bytes)
        .map_err(|_| AppError::RuntimeUnavailable)?;
    for (path, content) in rollouts {
        zip.start_file(path, options)
            .map_err(|_| AppError::RuntimeUnavailable)?;
        zip.write_all(content.as_bytes())
            .map_err(|_| AppError::RuntimeUnavailable)?;
    }
    let mut file = zip.finish().map_err(|_| AppError::RuntimeUnavailable)?;
    file.flush()
        .and_then(|()| file.sync_all())
        .map_err(|_| AppError::RuntimeUnavailable)?;
    fs::rename(&temporary, destination).map_err(|_| AppError::RuntimeUnavailable)?;
    Ok(())
}

fn import_history_path(
    source: &Path,
    import_home: &Path,
    warnings: &mut Vec<String>,
) -> AppResult<()> {
    if source.extension().and_then(|value| value.to_str()) == Some("zip") {
        return import_history_zip(source, import_home, warnings);
    }
    if source
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.ends_with(".jsonl"))
    {
        let bytes = fs::read(source).map_err(|_| AppError::LocalStateUnavailable)?;
        let session_id = import_rollout_bytes(import_home, &source.display().to_string(), &bytes)?;
        append_import_index_entry(import_home, &session_id, None, None)?;
        return Ok(());
    }
    warnings.push(format!(
        "{} 不是支持的会话历史文件，已跳过。",
        source.display()
    ));
    Ok(())
}

fn import_history_zip(
    source: &Path,
    import_home: &Path,
    warnings: &mut Vec<String>,
) -> AppResult<()> {
    let file = fs::File::open(source).map_err(|_| AppError::LocalStateUnavailable)?;
    let mut archive = ZipArchive::new(file).map_err(|_| AppError::LocalStateUnavailable)?;
    let manifest = read_history_zip_manifest(&mut archive, warnings);
    let manifest_by_rollout = manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .sessions
                .iter()
                .map(|session| (session.rollout_path.clone(), session.clone()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let manifest_by_id = manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .sessions
                .iter()
                .map(|session| (session.id.clone(), session.clone()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|_| AppError::LocalStateUnavailable)?;
        let name = file.name().to_owned();
        if !is_safe_export_rollout_path(&name) {
            continue;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| AppError::LocalStateUnavailable)?;
        let session_id = import_rollout_bytes(import_home, &name, &bytes)?;
        let manifest_session = manifest_by_rollout
            .get(&name)
            .or_else(|| manifest_by_id.get(&session_id));
        append_import_index_entry(
            import_home,
            &session_id,
            manifest_session.and_then(|session| session.title.clone()),
            manifest_session.map(|session| session.updated_at_ms),
        )?;
    }
    Ok(())
}

fn read_history_zip_manifest(
    archive: &mut ZipArchive<fs::File>,
    warnings: &mut Vec<String>,
) -> Option<HistoryExportManifest> {
    let mut file = archive.by_name("manifest.json").ok()?;
    let mut content = String::new();
    if file.read_to_string(&mut content).is_err() {
        warnings.push("导出包 manifest 暂不可读，已继续导入 rollout。".to_owned());
        return None;
    }
    match serde_json::from_str::<HistoryExportManifest>(&content) {
        Ok(manifest) if manifest.format == HISTORY_EXPORT_FORMAT => Some(manifest),
        Ok(_) => {
            warnings.push("导出包格式标识不兼容，已继续尝试导入 rollout。".to_owned());
            None
        }
        Err(_) => {
            warnings.push("导出包 manifest 不是有效 JSON，已继续导入 rollout。".to_owned());
            None
        }
    }
}

fn is_safe_export_rollout_path(name: &str) -> bool {
    name.starts_with("rollouts/")
        && name.ends_with(".jsonl")
        && !name.contains("..")
        && !name.contains('\\')
        && Path::new(name)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn import_rollout_bytes(import_home: &Path, source_name: &str, bytes: &[u8]) -> AppResult<String> {
    let text = std::str::from_utf8(bytes).map_err(|_| AppError::LocalStateUnavailable)?;
    let session_id = session_id_from_rollout_text(text)
        .or_else(|| session_id_from_filename(Path::new(source_name)))
        .ok_or(AppError::ValidationFailed)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let fingerprint = hex_hash(hasher.finalize().as_slice())
        .get(..12)
        .unwrap_or("unknown")
        .to_owned();
    let path = import_home
        .join("sessions/imported")
        .join(format!("rollout-imported-{fingerprint}-{session_id}.jsonl"));
    fs::write(path, bytes).map_err(|_| AppError::RuntimeUnavailable)?;
    Ok(session_id)
}

fn session_id_from_rollout_text(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .filter(is_session_meta)
            .and_then(|value| session_id_from_meta(&value))
    })
}

fn append_import_index_entry(
    import_home: &Path,
    session_id: &str,
    title: Option<String>,
    updated_at_ms: Option<i64>,
) -> AppResult<()> {
    let path = import_home.join("session_index.jsonl");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|_| AppError::RuntimeUnavailable)?;
    let value = json!({
        "id": session_id,
        "thread_name": title.unwrap_or_else(|| "导入的 Codex 会话".to_owned()),
        "updated_at": rfc3339_ms(updated_at_ms.unwrap_or_else(timestamp_ms)),
    });
    serde_json::to_writer(&mut file, &value).map_err(|_| AppError::Internal)?;
    file.write_all(b"\n")
        .map_err(|_| AppError::RuntimeUnavailable)?;
    Ok(())
}

fn imported_session_ids(
    import_home: &Path,
    warnings: &mut Vec<String>,
) -> AppResult<HashSet<String>> {
    let home = CodexHome {
        id: "import".to_owned(),
        kind: "history_import".to_owned(),
        label: "导入历史".to_owned(),
        path: import_home.to_path_buf(),
        sync_target: false,
    };
    let index = read_session_index(import_home, warnings);
    let mut ids = HashSet::new();
    for path in rollout_paths(import_home, warnings) {
        match read_rollout_snapshot(&home, 0, &path, &index) {
            Ok(Some(snapshot)) => {
                ids.insert(snapshot.session_id);
            }
            Ok(None) => {}
            Err(error) => warnings.push(format!("{} 导入扫描失败：{}", path.display(), error)),
        }
    }
    Ok(ids)
}

fn collect_homes(
    repository: &Repository,
    data_dir: &Path,
    extra_target: Option<ExtraTargetHome>,
) -> AppResult<Vec<CodexHome>> {
    let mut homes = Vec::new();
    let default = default_codex_home()?;
    homes.push(CodexHome {
        id: "default".to_owned(),
        kind: "default".to_owned(),
        label: "默认 Codex".to_owned(),
        path: default,
        sync_target: true,
    });
    for stored in repository.list_profiles()? {
        homes.push(CodexHome {
            id: format!("profile:{}", stored.profile.id),
            kind: "profile".to_owned(),
            label: format!("档案：{}", stored.profile.alias),
            path: data_dir.join("runtimes").join(&stored.profile.id),
            sync_target: true,
        });
    }
    collect_child_codex_homes(
        &mut homes,
        &data_dir.join("collaboration-contexts"),
        "collaboration_context",
        "协作上下文",
        true,
    );
    collect_child_codex_homes(
        &mut homes,
        &data_dir.join("collaboration-sessions"),
        "collaboration_session",
        "协作会话",
        false,
    );
    if let Some(extra) = extra_target {
        homes.push(CodexHome {
            id: extra.id,
            kind: extra.kind,
            label: extra.label,
            path: extra.path,
            sync_target: extra.sync_target,
        });
    }
    dedupe_homes(homes)
}

fn collect_child_codex_homes(
    homes: &mut Vec<CodexHome>,
    root: &Path,
    kind: &str,
    label_prefix: &str,
    sync_target: bool,
) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let home = path.join("codex-home");
        if home.exists() {
            homes.push(CodexHome {
                id: format!("{kind}:{id}"),
                kind: kind.to_owned(),
                label: format!("{label_prefix}：{}", short_id(id)),
                path: home,
                sync_target,
            });
        }
    }
}

fn dedupe_homes(homes: Vec<CodexHome>) -> AppResult<Vec<CodexHome>> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for home in homes {
        let key = fs::canonicalize(&home.path)
            .unwrap_or_else(|_| home.path.clone())
            .display()
            .to_string();
        if seen.insert(key) {
            deduped.push(home);
        }
    }
    Ok(deduped)
}

fn prioritize_target_homes(homes: &mut [CodexHome], priority_home: Option<&Path>) {
    let Some(priority_home) = priority_home else {
        return;
    };
    let priority_key = canonical_path_key(priority_home);
    homes.sort_by_key(|home| {
        if home.id == "default" {
            0
        } else if canonical_path_key(&home.path) == priority_key {
            1
        } else {
            2
        }
    });
}

fn canonical_path_key(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn rollout_paths(home: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root_name in ["sessions", "archived_sessions"] {
        collect_rollout_paths(&home.join(root_name), &mut paths, warnings);
    }
    paths.sort();
    paths
}

fn collect_rollout_paths(root: &Path, paths: &mut Vec<PathBuf>, warnings: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            warnings.push(format!("{} 目录项暂不可读。", root.display()));
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_paths(&path, paths, warnings);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        {
            paths.push(path);
        }
    }
}

fn read_session_index(home: &Path, warnings: &mut Vec<String>) -> HashMap<String, IndexEntry> {
    let path = home.join("session_index.jsonl");
    let Ok(content) = fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let mut index = HashMap::new();
    for (line_number, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            warnings.push(format!(
                "{}:{} index 行不是有效 JSON，已保留但跳过解析。",
                path.display(),
                line_number + 1
            ));
            continue;
        };
        let Some(id) = string_field(&value, &["id", "session_id", "conversation_id"]) else {
            continue;
        };
        index.insert(
            id,
            IndexEntry {
                title: string_field(&value, &["thread_name", "title", "summary"]),
                updated_at_ms: timestamp_field(
                    &value,
                    &["updated_at", "updatedAt", "updated_at_ms"],
                ),
            },
        );
    }
    index
}

fn read_sqlite_thread_index(
    home: &Path,
    warnings: &mut Vec<String>,
) -> HashMap<String, SqliteThreadEntry> {
    let path = home.join("state_5.sqlite");
    if !path.exists() {
        return HashMap::new();
    }
    let connection = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(connection) => connection,
        Err(_) => {
            warnings.push(format!(
                "{} sqlite 暂不可读，已跳过列表元数据。",
                path.display()
            ));
            return HashMap::new();
        }
    };
    let Some(columns) = sqlite_thread_columns(&connection) else {
        warnings.push(format!(
            "{} threads 表暂不可读，已跳过列表元数据。",
            path.display()
        ));
        return HashMap::new();
    };
    if !columns.contains("id") {
        return HashMap::new();
    }
    let select_title = if columns.contains("title") {
        "title"
    } else {
        "NULL AS title"
    };
    let select_cwd = if columns.contains("cwd") {
        "cwd"
    } else {
        "NULL AS cwd"
    };
    let select_updated_at = if columns.contains("updated_at") {
        "updated_at"
    } else if columns.contains("created_at") {
        "created_at AS updated_at"
    } else {
        "NULL AS updated_at"
    };
    let select_archived = if columns.contains("archived") {
        "archived"
    } else {
        "NULL AS archived"
    };
    let query = format!(
        "SELECT id, {select_title}, {select_cwd}, {select_updated_at}, {select_archived} FROM threads"
    );
    let mut statement = match connection.prepare(&query) {
        Ok(statement) => statement,
        Err(_) => {
            warnings.push(format!(
                "{} threads 表结构不兼容，已跳过列表元数据。",
                path.display()
            ));
            return HashMap::new();
        }
    };
    let rows = match statement.query_map([], |row| {
        let id = row.get::<_, String>(0)?;
        let title = row.get::<_, Option<String>>(1)?;
        let cwd = row.get::<_, Option<String>>(2)?;
        let updated_at_ms = row.get::<_, Option<i64>>(3)?.map(normalize_epoch_ms);
        let archived = row.get::<_, Option<i64>>(4)?.map(|value| value != 0);
        Ok((
            id,
            SqliteThreadEntry {
                title: title
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty()),
                cwd: cwd
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty()),
                updated_at_ms,
                archived,
            },
        ))
    }) {
        Ok(rows) => rows,
        Err(_) => {
            warnings.push(format!(
                "{} threads 表暂不可读，已跳过列表元数据。",
                path.display()
            ));
            return HashMap::new();
        }
    };
    rows.flatten().collect()
}

fn read_rollout_list_snapshot(
    home: &CodexHome,
    source_rank: usize,
    path: &Path,
    index: &HashMap<String, IndexEntry>,
    sqlite: &HashMap<String, SqliteThreadEntry>,
) -> AppResult<Option<HistoryListSnapshot>> {
    let metadata = fs::metadata(path).map_err(|_| AppError::LocalStateUnavailable)?;
    let file_len = metadata.len();
    let modified_at_ms = metadata_modified_ms(&metadata);
    let first_line = read_first_rollout_line(path)?;
    let first_value = first_line
        .as_deref()
        .and_then(|line| serde_json::from_str::<Value>(line).ok());
    let first_meta = first_value.as_ref().filter(|value| is_session_meta(value));
    let Some(session_id) = first_meta
        .and_then(session_id_from_meta)
        .or_else(|| session_id_from_filename(path))
    else {
        return Ok(None);
    };
    let index_entry = index.get(&session_id);
    let sqlite_entry = sqlite.get(&session_id);
    let mut updated_at_ms = modified_at_ms;
    if let Some(index_updated) = index_entry.and_then(|entry| entry.updated_at_ms) {
        updated_at_ms = updated_at_ms.max(index_updated);
    }
    if let Some(sqlite_updated) = sqlite_entry.and_then(|entry| entry.updated_at_ms) {
        updated_at_ms = updated_at_ms.max(sqlite_updated);
    }
    let relative_path = path
        .strip_prefix(&home.path)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| {
            path.file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(format!("rollout-{session_id}.jsonl")))
        });
    let archived_from_path = relative_path
        .components()
        .next()
        .is_some_and(|component| component.as_os_str() == "archived_sessions");
    let archived = sqlite_entry
        .and_then(|entry| entry.archived)
        .unwrap_or(archived_from_path);
    let sha256 = list_rollout_fingerprint(
        &session_id,
        file_len,
        Some(modified_at_ms),
        first_line.as_deref(),
        false,
    );
    let normalized_sha256 =
        list_rollout_fingerprint(&session_id, file_len, None, first_line.as_deref(), true);
    Ok(Some(HistoryListSnapshot {
        session_id,
        title: index_entry
            .and_then(|entry| entry.title.clone())
            .or_else(|| sqlite_entry.and_then(|entry| entry.title.clone())),
        cwd: first_meta
            .and_then(cwd_from_meta)
            .or_else(|| sqlite_entry.and_then(|entry| entry.cwd.clone())),
        updated_at_ms,
        event_count: 0,
        sha256,
        normalized_sha256,
        rollout_path: path.to_path_buf(),
        archived,
        home: home.clone(),
        source_rank,
    }))
}

fn read_first_rollout_line(path: &Path) -> AppResult<Option<String>> {
    let file = fs::File::open(path).map_err(|_| AppError::LocalStateUnavailable)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .map_err(|_| AppError::LocalStateUnavailable)?;
    if bytes == 0 {
        return Ok(None);
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Some(line))
}

fn list_rollout_fingerprint(
    session_id: &str,
    file_len: u64,
    modified_at_ms: Option<i64>,
    first_line: Option<&str>,
    normalize_meta: bool,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update(file_len.to_le_bytes());
    if let Some(modified_at_ms) = modified_at_ms {
        hasher.update(modified_at_ms.to_le_bytes());
    }
    if let Some(first_line) = first_line {
        let normalized = if normalize_meta {
            serde_json::from_str::<Value>(first_line)
                .ok()
                .filter(is_session_meta)
                .map(|mut value| {
                    set_session_meta_provider(&mut value, "");
                    serde_json::to_string(&value).unwrap_or_else(|_| first_line.to_owned())
                })
        } else {
            None
        };
        hasher.update(normalized.as_deref().unwrap_or(first_line).as_bytes());
    }
    hex_hash(hasher.finalize().as_slice())
}

fn read_rollout_snapshot(
    home: &CodexHome,
    source_rank: usize,
    path: &Path,
    index: &HashMap<String, IndexEntry>,
) -> AppResult<Option<RolloutSnapshot>> {
    let file = fs::File::open(path).map_err(|_| AppError::LocalStateUnavailable)?;
    let mut lines = Vec::new();
    let mut session_id = None;
    let mut cwd = None;
    let mut updated_at_ms = file_modified_ms(path);
    for (line_index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|_| AppError::LocalStateUnavailable)?;
        let value = serde_json::from_str::<Value>(&line).ok();
        let line_timestamp = value
            .as_ref()
            .and_then(event_timestamp_ms)
            .unwrap_or(updated_at_ms.saturating_add(line_index as i64));
        updated_at_ms = updated_at_ms.max(line_timestamp);
        if let Some(value) = value.as_ref().filter(|value| is_session_meta(value)) {
            session_id = session_id.or_else(|| session_id_from_meta(value));
            cwd = cwd.or_else(|| cwd_from_meta(value));
        }
        lines.push(RolloutLine {
            raw: line,
            value,
            timestamp_ms: line_timestamp,
            source_rank,
            line_index,
        });
    }
    let Some(session_id) = session_id.or_else(|| session_id_from_filename(path)) else {
        return Ok(None);
    };
    let index_entry = index.get(&session_id);
    let title = index_entry.and_then(|entry| entry.title.clone());
    if let Some(index_updated) = index_entry.and_then(|entry| entry.updated_at_ms) {
        updated_at_ms = updated_at_ms.max(index_updated);
    }
    let relative_path = path
        .strip_prefix(&home.path)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| {
            path.file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(format!("rollout-{session_id}.jsonl")))
        });
    Ok(Some(RolloutSnapshot {
        session_id,
        title,
        cwd,
        updated_at_ms,
        rollout_path: path.to_path_buf(),
        relative_path,
        home: home.clone(),
        source_rank,
        lines,
    }))
}

fn report_from_list_scan(scan: ListScanResult, page: HistoryListPage) -> CodexHistoryReport {
    let scanned_at_ms = timestamp_ms();
    let groups = list_snapshots_by_session(&scan.snapshots);
    let target_home_ids = scan
        .homes
        .iter()
        .filter(|home| home.sync_target)
        .map(|home| home.id.as_str())
        .collect::<HashSet<_>>();
    let mut home_counts = HashMap::<String, usize>::new();
    for snapshot in &scan.snapshots {
        *home_counts.entry(snapshot.home.id.clone()).or_default() += 1;
    }
    let homes = scan
        .homes
        .iter()
        .map(|home| CodexHistoryHomeSummary {
            id: home.id.clone(),
            kind: home.kind.clone(),
            label: home.label.clone(),
            path: home.path.display().to_string(),
            sync_target: home.sync_target,
            session_count: home_counts.get(&home.id).copied().unwrap_or_default(),
        })
        .collect();
    let mut all_sessions = groups
        .into_iter()
        .map(|(id, snapshots)| {
            let primary = primary_list_snapshot(&snapshots);
            let project = project_identity(primary.cwd.as_deref());
            let source_home_ids = snapshots
                .iter()
                .map(|snapshot| snapshot.home.id.as_str())
                .collect::<HashSet<_>>();
            let missing_target_count = target_home_ids.difference(&source_home_ids).count();
            let divergent_source_count = snapshots
                .iter()
                .map(|snapshot| snapshot.normalized_sha256.as_str())
                .collect::<HashSet<_>>()
                .len()
                .saturating_sub(1);
            let status = if divergent_source_count > 0 {
                "conflict"
            } else if missing_target_count > 0 {
                "missing"
            } else {
                "consistent"
            };
            let mut sources = snapshots
                .iter()
                .map(|snapshot| CodexHistorySourceSummary {
                    home_id: snapshot.home.id.clone(),
                    home_label: snapshot.home.label.clone(),
                    home_kind: snapshot.home.kind.clone(),
                    rollout_path: snapshot.rollout_path.display().to_string(),
                    archived: snapshot.archived,
                    updated_at_ms: snapshot.updated_at_ms,
                    event_count: snapshot.event_count,
                    sha256: snapshot.sha256.clone(),
                })
                .collect::<Vec<_>>();
            sources.sort_by(|left, right| {
                right
                    .updated_at_ms
                    .cmp(&left.updated_at_ms)
                    .then_with(|| left.home_label.cmp(&right.home_label))
            });
            CodexHistorySessionSummary {
                id,
                title: primary.title.clone(),
                cwd: primary.cwd.clone(),
                project_id: project.id,
                project_name: project.name,
                updated_at_ms: primary.updated_at_ms,
                status: status.to_owned(),
                source_count: snapshots.len(),
                missing_target_count,
                divergent_source_count,
                sources,
            }
        })
        .collect::<Vec<_>>();
    all_sessions.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    let projects = projects_from_sessions(&all_sessions);
    let selected_project_id = selected_project_id(&projects, page.project_id.as_deref());
    let filtered_sessions = all_sessions
        .into_iter()
        .filter(|session| {
            selected_project_id
                .as_deref()
                .is_none_or(|project_id| session.project_id == project_id)
        })
        .collect::<Vec<_>>();
    let total_sessions = filtered_sessions.len();
    let sessions = filtered_sessions
        .into_iter()
        .skip(page.offset)
        .take(page.limit)
        .collect();
    CodexHistoryReport {
        scanned_at_ms,
        limit: page.limit,
        offset: page.offset,
        total_sessions,
        selected_project_id,
        projects,
        homes,
        sessions,
        warnings: scan.warnings,
    }
}

fn projects_from_sessions(
    sessions: &[CodexHistorySessionSummary],
) -> Vec<CodexHistoryProjectSummary> {
    let mut projects = HashMap::<String, CodexHistoryProjectSummary>::new();
    for session in sessions {
        let entry = projects
            .entry(session.project_id.clone())
            .or_insert_with(|| CodexHistoryProjectSummary {
                cwd: project_identity(session.cwd.as_deref()).cwd,
                id: session.project_id.clone(),
                name: session.project_name.clone(),
                session_count: 0,
                consistent_count: 0,
                missing_count: 0,
                conflict_count: 0,
                needs_repair_count: 0,
                updated_at_ms: 0,
            });
        entry.session_count += 1;
        entry.updated_at_ms = entry.updated_at_ms.max(session.updated_at_ms);
        match session.status.as_str() {
            "consistent" => entry.consistent_count += 1,
            "missing" => entry.missing_count += 1,
            "conflict" => entry.conflict_count += 1,
            _ => entry.needs_repair_count += 1,
        }
    }
    let mut projects = projects.into_values().collect::<Vec<_>>();
    projects.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.name.cmp(&right.name))
    });
    projects
}

fn selected_project_id(
    projects: &[CodexHistoryProjectSummary],
    requested: Option<&str>,
) -> Option<String> {
    if let Some(requested) = requested {
        if projects.iter().any(|project| project.id == requested) {
            return Some(requested.to_owned());
        }
    }
    projects.first().map(|project| project.id.clone())
}

fn project_identity(cwd: Option<&str>) -> ProjectIdentity {
    let Some(cwd) = cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) else {
        return ProjectIdentity {
            id: UNCATEGORIZED_PROJECT_ID.to_owned(),
            name: UNCATEGORIZED_PROJECT_NAME.to_owned(),
            cwd: None,
        };
    };
    let normalized = Path::new(cwd)
        .components()
        .collect::<PathBuf>()
        .display()
        .to_string();
    let cwd = if normalized.is_empty() {
        cwd.to_owned()
    } else {
        normalized
    };
    let name = Path::new(&cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| cwd.clone());
    ProjectIdentity {
        id: format!("project:{}", stable_path_id(Path::new(&cwd))),
        name,
        cwd: Some(cwd),
    }
}

fn snapshots_by_session(snapshots: &[RolloutSnapshot]) -> HashMap<String, Vec<RolloutSnapshot>> {
    let mut groups = HashMap::<String, Vec<RolloutSnapshot>>::new();
    for snapshot in snapshots {
        groups
            .entry(snapshot.session_id.clone())
            .or_default()
            .push(snapshot.clone());
    }
    groups
}

fn list_snapshots_by_session(
    snapshots: &[HistoryListSnapshot],
) -> HashMap<String, Vec<HistoryListSnapshot>> {
    let mut groups = HashMap::<String, Vec<HistoryListSnapshot>>::new();
    for snapshot in snapshots {
        groups
            .entry(snapshot.session_id.clone())
            .or_default()
            .push(snapshot.clone());
    }
    groups
}

struct CanonicalRollout {
    session_id: String,
    title: Option<String>,
    cwd: Option<String>,
    updated_at_ms: i64,
    primary: RolloutSnapshot,
    lines: Vec<RolloutLine>,
}

fn canonical_rollout(session_id: &str, snapshots: &[RolloutSnapshot]) -> CanonicalRollout {
    let primary = primary_snapshot(snapshots).clone();
    let mut first_meta = primary
        .lines
        .iter()
        .find(|line| line.value.as_ref().is_some_and(is_session_meta))
        .cloned()
        .or_else(|| {
            snapshots.iter().find_map(|snapshot| {
                snapshot
                    .lines
                    .iter()
                    .find(|line| line.value.as_ref().is_some_and(is_session_meta))
                    .cloned()
            })
        });
    if first_meta.is_none() {
        first_meta = primary.lines.first().cloned();
    }
    let mut seen = HashSet::new();
    let mut event_lines = Vec::new();
    for snapshot in snapshots {
        for line in &snapshot.lines {
            if line.value.as_ref().is_some_and(is_session_meta) {
                continue;
            }
            let key = normalized_line_key(line);
            if seen.insert(key) {
                event_lines.push(line.clone());
            }
        }
    }
    event_lines.sort_by(|left, right| {
        left.timestamp_ms
            .cmp(&right.timestamp_ms)
            .then_with(|| left.source_rank.cmp(&right.source_rank))
            .then_with(|| left.line_index.cmp(&right.line_index))
    });
    let mut lines = Vec::new();
    if let Some(meta) = first_meta {
        lines.push(meta);
    }
    lines.extend(event_lines);
    CanonicalRollout {
        session_id: session_id.to_owned(),
        title: primary.title.clone(),
        cwd: primary.cwd.clone(),
        updated_at_ms: primary.updated_at_ms,
        primary,
        lines,
    }
}

fn primary_snapshot(snapshots: &[RolloutSnapshot]) -> &RolloutSnapshot {
    snapshots
        .iter()
        .max_by(|left, right| {
            left.updated_at_ms
                .cmp(&right.updated_at_ms)
                .then_with(|| right.source_rank.cmp(&left.source_rank))
        })
        .expect("session groups are never empty")
}

fn primary_list_snapshot(snapshots: &[HistoryListSnapshot]) -> &HistoryListSnapshot {
    snapshots
        .iter()
        .max_by(|left, right| {
            left.updated_at_ms
                .cmp(&right.updated_at_ms)
                .then_with(|| right.source_rank.cmp(&left.source_rank))
        })
        .expect("session groups are never empty")
}

fn target_path_for_session(
    home: &CodexHome,
    snapshots: &[RolloutSnapshot],
    primary: &RolloutSnapshot,
) -> PathBuf {
    snapshots
        .iter()
        .find(|snapshot| snapshot.home.id == home.id)
        .map(|snapshot| snapshot.rollout_path.clone())
        .unwrap_or_else(|| home.path.join(&primary.relative_path))
}

fn render_rollout_lines(lines: &[RolloutLine], provider: &str) -> String {
    let mut rendered = String::new();
    for line in lines {
        if let Some(value) = line
            .value
            .as_ref()
            .filter(|value| is_session_meta(value))
            .cloned()
        {
            let mut value = value;
            set_session_meta_provider(&mut value, provider);
            rendered.push_str(&serde_json::to_string(&value).unwrap_or_else(|_| line.raw.clone()));
        } else {
            rendered.push_str(&line.raw);
        }
        rendered.push('\n');
    }
    rendered
}

fn normalized_line_key(line: &RolloutLine) -> String {
    if let Some(mut value) = line.value.clone() {
        if is_session_meta(&value) {
            set_session_meta_provider(&mut value, "");
        }
        serde_json::to_string(&value).unwrap_or_else(|_| line.raw.clone())
    } else {
        line.raw.clone()
    }
}

fn upsert_session_index(
    home: &CodexHome,
    canonical: &CanonicalRollout,
    target_path: &Path,
    backup_root: &Path,
    stats: &mut SyncStats,
    warnings: &mut Vec<String>,
) -> AppResult<bool> {
    let path = home.path.join("session_index.jsonl");
    let current = fs::read_to_string(&path).unwrap_or_default();
    let mut lines = Vec::new();
    let mut found = false;
    for (line_number, line) in current.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(mut value) => {
                if string_field(&value, &["id", "session_id", "conversation_id"]).as_deref()
                    == Some(canonical.session_id.as_str())
                {
                    update_index_value(&mut value, canonical, target_path);
                    found = true;
                }
                lines.push(serde_json::to_string(&value).map_err(|_| AppError::Internal)?);
            }
            Err(_) => {
                warnings.push(format!(
                    "{}:{} index 行不是有效 JSON，已原样保留。",
                    path.display(),
                    line_number + 1
                ));
                lines.push(line.to_owned());
            }
        }
    }
    if !found {
        let mut value = json!({
            "id": canonical.session_id,
            "thread_name": canonical
                .title
                .clone()
                .unwrap_or_else(|| "未命名 Codex 会话".to_owned()),
            "updated_at": rfc3339_ms(canonical.updated_at_ms),
        });
        update_index_value(&mut value, canonical, target_path);
        lines.push(serde_json::to_string(&value).map_err(|_| AppError::Internal)?);
    }
    let next = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    write_if_changed(&path, next.as_bytes(), backup_root, home, stats)
}

fn update_index_value(value: &mut Value, canonical: &CanonicalRollout, target_path: &Path) {
    if !value.is_object() {
        *value = json!({});
    }
    let object = value.as_object_mut().expect("object just ensured");
    object.insert("id".to_owned(), Value::String(canonical.session_id.clone()));
    object.insert(
        "thread_name".to_owned(),
        Value::String(
            canonical
                .title
                .clone()
                .unwrap_or_else(|| "未命名 Codex 会话".to_owned()),
        ),
    );
    object.insert(
        "updated_at".to_owned(),
        Value::String(rfc3339_ms(canonical.updated_at_ms)),
    );
    object.insert(
        "rollout_path".to_owned(),
        Value::String(target_path.display().to_string()),
    );
    if let Some(cwd) = canonical.cwd.as_deref() {
        object.insert("cwd".to_owned(), Value::String(cwd.to_owned()));
    }
}

fn repair_sqlite_thread(
    home: &CodexHome,
    canonical: &CanonicalRollout,
    target_path: &Path,
    provider: &str,
    backup_root: &Path,
    stats: &mut SyncStats,
    warnings: &mut Vec<String>,
) -> AppResult<bool> {
    let path = home.path.join("state_5.sqlite");
    if !path.exists() {
        return Ok(false);
    }
    let connection = match Connection::open(&path) {
        Ok(connection) => connection,
        Err(_) => {
            warnings.push(format!("{} sqlite 暂不可读，已跳过。", path.display()));
            return Ok(false);
        }
    };
    if !sqlite_threads_table_supported(&connection) {
        warnings.push(format!("{} threads 表结构不兼容，已跳过。", path.display()));
        return Ok(false);
    }
    backup_file(&path, backup_root, home, stats)?;
    let updated_at = canonical.updated_at_ms / 1000;
    let title = canonical.title.as_deref().unwrap_or("未命名 Codex 会话");
    let cwd = canonical.cwd.as_deref().unwrap_or("");
    let archived = if target_path
        .strip_prefix(&home.path)
        .ok()
        .and_then(|path| path.components().next())
        .is_some_and(|component| component.as_os_str() == "archived_sessions")
    {
        1
    } else {
        0
    };
    connection
        .execute(
            "INSERT INTO threads
             (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, archived)
             VALUES (?1, ?2, ?3, ?4, 'local', ?5, ?6, ?7, 'workspace-write', 'on-request', ?8)
             ON CONFLICT(id) DO UPDATE SET
               rollout_path = excluded.rollout_path,
               updated_at = excluded.updated_at,
               model_provider = excluded.model_provider,
               cwd = excluded.cwd,
               title = excluded.title,
               archived = excluded.archived",
            params![
                canonical.session_id,
                target_path.display().to_string(),
                updated_at,
                updated_at,
                provider,
                cwd,
                title,
                archived,
            ],
        )
        .map_err(|_| AppError::LocalStateUnavailable)?;
    stats.files_written += 1;
    stats.changed_home_ids.insert(home.id.clone());
    Ok(true)
}

fn sqlite_threads_table_supported(connection: &Connection) -> bool {
    let Some(columns) = sqlite_thread_columns(connection) else {
        return false;
    };
    [
        "id",
        "rollout_path",
        "created_at",
        "updated_at",
        "source",
        "model_provider",
        "cwd",
        "title",
        "sandbox_policy",
        "approval_mode",
        "archived",
    ]
    .iter()
    .all(|column| columns.contains(*column))
}

fn sqlite_thread_columns(connection: &Connection) -> Option<HashSet<String>> {
    let mut statement = connection.prepare("PRAGMA table_info(threads)").ok()?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .ok()?;
    Some(rows.flatten().collect())
}

fn update_global_state(
    home: &CodexHome,
    cwds: &[String],
    backup_root: &Path,
    stats: &mut SyncStats,
    warnings: &mut Vec<String>,
) -> AppResult<bool> {
    let unique_cwds = cwds
        .iter()
        .filter(|cwd| !cwd.trim().is_empty())
        .cloned()
        .collect::<HashSet<_>>();
    if unique_cwds.is_empty() {
        return Ok(false);
    }
    let path = home.path.join(".codex-global-state.json");
    let mut value = match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str::<Value>(&content).unwrap_or_else(|_| {
            warnings.push(format!(
                "{} 不是有效 JSON，将用对象结构修复。",
                path.display()
            ));
            json!({})
        }),
        Err(_) => json!({}),
    };
    if !value.is_object() {
        value = json!({});
    }
    let object = value.as_object_mut().expect("object just ensured");
    let entry = object
        .entry("active-workspace-roots")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entry.is_array() {
        *entry = Value::Array(Vec::new());
    }
    let roots = entry.as_array_mut().expect("array just ensured");
    let mut changed = false;
    let existing = roots
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    for cwd in unique_cwds {
        if !existing.contains(&cwd) {
            roots.push(Value::String(cwd));
            changed = true;
        }
    }
    if !changed {
        return Ok(false);
    }
    let next = serde_json::to_vec_pretty(&value).map_err(|_| AppError::Internal)?;
    write_if_changed(&path, &next, backup_root, home, stats)
}

fn write_if_changed(
    path: &Path,
    content: &[u8],
    backup_root: &Path,
    home: &CodexHome,
    stats: &mut SyncStats,
) -> AppResult<bool> {
    let current = fs::read(path).unwrap_or_default();
    if current == content {
        return Ok(false);
    }
    if path.exists() {
        backup_file(path, backup_root, home, stats)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
    }
    let temporary = path.with_file_name(format!(
        ".{}.history-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("codex-history"),
        Uuid::new_v4()
    ));
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| AppError::RuntimeUnavailable)?;
        file.write_all(content)
            .and_then(|()| file.sync_all())
            .map_err(|_| AppError::RuntimeUnavailable)?;
    }
    fs::rename(&temporary, path).map_err(|_| AppError::RuntimeUnavailable)?;
    stats.files_written += 1;
    stats.changed_home_ids.insert(home.id.clone());
    Ok(true)
}

fn backup_file(
    path: &Path,
    backup_root: &Path,
    home: &CodexHome,
    stats: &mut SyncStats,
) -> AppResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let relative = path
        .strip_prefix(&home.path)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| {
            path.file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("unknown"))
        });
    let destination = backup_root
        .join(sanitize_path_segment(&home.id))
        .join(relative);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|_| AppError::RuntimeUnavailable)?;
    }
    fs::copy(path, destination).map_err(|_| AppError::RuntimeUnavailable)?;
    stats.files_backed_up += 1;
    Ok(())
}

fn model_provider_for_home(home: &Path) -> String {
    let config = home.join("config.toml");
    fs::read_to_string(config)
        .ok()
        .and_then(|content| content.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|document| {
            document
                .get("model_provider")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
        .filter(|provider| !provider.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL_PROVIDER.to_owned())
}

fn rebuild_thread_metadata(home: &Path) -> AppResult<()> {
    let mut command = Command::new("codex");
    command
        .args(["app-server", "--stdio"])
        .env("CODEX_HOME", home)
        .env_remove("CODEX_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::CodexCliMissing
        } else {
            AppError::RuntimeUnavailable
        }
    })?;
    let result = (|| -> AppResult<()> {
        let mut stdin = child.stdin.take().ok_or(AppError::RuntimeUnavailable)?;
        for request in [
            json!({
                "method": "initialize",
                "id": 1,
                "params": {
                    "clientInfo": {
                        "name": "codex-relay",
                        "title": "Codex Relay",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {}
                }
            }),
            json!({"method": "initialized", "params": {}}),
            json!({"method": "thread/list", "id": 2, "params": {}}),
        ] {
            serde_json::to_writer(&mut stdin, &request)
                .map_err(|_| AppError::RuntimeUnavailable)?;
            stdin
                .write_all(b"\n")
                .map_err(|_| AppError::RuntimeUnavailable)?;
        }
        stdin.flush().map_err(|_| AppError::RuntimeUnavailable)?;
        drop(stdin);
        let stdout = child.stdout.take().ok_or(AppError::RuntimeUnavailable)?;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + APP_SERVER_TIMEOUT;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let line = receiver
                .recv_timeout(remaining)
                .map_err(|_| AppError::RuntimeUnavailable)?;
            let response: Value =
                serde_json::from_str(&line).map_err(|_| AppError::RuntimeUnavailable)?;
            if response.get("id") != Some(&json!(2)) {
                continue;
            }
            if response.get("error").is_some() {
                return Err(AppError::RuntimeUnavailable);
            }
            return Ok(());
        }
        Err(AppError::RuntimeUnavailable)
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn is_session_meta(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("session_meta")
}

fn session_id_from_meta(value: &Value) -> Option<String> {
    value
        .get("payload")
        .and_then(|payload| string_field(payload, &["id", "session_id", "conversation_id"]))
        .or_else(|| string_field(value, &["id", "session_id", "conversation_id"]))
}

fn cwd_from_meta(value: &Value) -> Option<String> {
    value
        .get("payload")
        .and_then(|payload| string_field(payload, &["cwd", "working_directory"]))
        .or_else(|| string_field(value, &["cwd", "working_directory"]))
}

fn set_session_meta_provider(value: &mut Value, provider: &str) {
    if let Some(payload) = value.get_mut("payload").and_then(Value::as_object_mut) {
        payload.insert(
            "model_provider".to_owned(),
            Value::String(provider.to_owned()),
        );
    }
}

fn event_timestamp_ms(value: &Value) -> Option<i64> {
    timestamp_field(
        value,
        &[
            "timestamp",
            "created_at",
            "createdAt",
            "updated_at",
            "updatedAt",
            "started_at",
            "startedAt",
        ],
    )
    .or_else(|| {
        value.get("payload").and_then(|payload| {
            timestamp_field(
                payload,
                &[
                    "timestamp",
                    "created_at",
                    "createdAt",
                    "updated_at",
                    "updatedAt",
                    "started_at",
                    "startedAt",
                ],
            )
        })
    })
}

fn timestamp_field(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(timestamp_value_ms))
}

fn timestamp_value_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().map(normalize_epoch_ms),
        Value::String(text) => parse_timestamp_ms(text),
        _ => None,
    }
}

fn parse_timestamp_ms(text: &str) -> Option<i64> {
    text.parse::<i64>()
        .ok()
        .map(normalize_epoch_ms)
        .or_else(|| {
            DateTime::parse_from_rfc3339(text)
                .ok()
                .map(|timestamp| timestamp.timestamp_millis())
        })
}

fn normalize_epoch_ms(value: i64) -> i64 {
    if value.abs() < 10_000_000_000 {
        value.saturating_mul(1000)
    } else {
        value
    }
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn session_id_from_filename(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let without_prefix = name.strip_prefix("rollout-")?;
    let without_suffix = without_prefix.strip_suffix(".jsonl")?;
    if Uuid::parse_str(without_suffix).is_ok() {
        return Some(without_suffix.to_owned());
    }
    if without_suffix.len() < 36 {
        return None;
    }
    (0..=without_suffix.len() - 36).rev().find_map(|start| {
        let candidate = &without_suffix[start..start + 36];
        Uuid::parse_str(candidate)
            .ok()
            .map(|_| candidate.to_owned())
    })
}

fn file_modified_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .map(|metadata| metadata_modified_ms(&metadata))
        .unwrap_or_else(|_| timestamp_ms())
}

fn metadata_modified_ms(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().try_into().unwrap_or(i64::MAX))
        .unwrap_or_else(timestamp_ms)
}

fn rfc3339_ms(timestamp_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn hex_hash(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn stable_path_id(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.display().to_string().as_bytes());
    hex_hash(hasher.finalize().as_slice())
        .get(..12)
        .unwrap_or("unknown")
        .to_owned()
}

fn sanitize_path_segment(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex as TestMutex, OnceLock};

    use crate::{
        database::{Repository, StoredProfile},
        domain::{CodexAuthMode, GatewayProvider, GatewayWireApi, MaskedProfile, ProfileKind},
    };

    fn env_lock() -> &'static TestMutex<()> {
        static LOCK: OnceLock<TestMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| TestMutex::new(()))
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("codex-history-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn repository(root: &Path) -> Repository {
        Repository::open(&root.join("relay.sqlite")).unwrap()
    }

    fn insert_profile(repository: &Repository, id: &str, alias: &str) {
        let profile = MaskedProfile {
            id: id.to_owned(),
            alias: alias.to_owned(),
            kind: ProfileKind::CodexOauth,
            base_url: None,
            provider: GatewayProvider::OpenAi,
            wire_api: GatewayWireApi::Responses,
            enabled: true,
            in_pool: false,
            priority: 0,
            weight: 1,
            models: Vec::new(),
            model_mappings: Vec::new(),
            health: "healthy".to_owned(),
            cooldown_until_ms: None,
            credential_configured: true,
            auth_mode: CodexAuthMode::OAuth,
            codex_oauth_profile_id: None,
            is_current: false,
            account: None,
            validation_status: "unknown".to_owned(),
            validated_at_ms: None,
            validation_message: None,
        };
        repository
            .insert_profile(&StoredProfile {
                profile,
                secret_ref: Some(format!("profile:{id}")),
                credential_fingerprint: None,
            })
            .unwrap();
    }

    fn rollout(home: &Path, session_id: &str, title: &str, event: &str) -> PathBuf {
        rollout_with_cwd(home, session_id, title, event, "/work")
    }

    fn rollout_with_cwd(
        home: &Path,
        session_id: &str,
        title: &str,
        event: &str,
        cwd: &str,
    ) -> PathBuf {
        let path = home
            .join("sessions")
            .join("2026")
            .join("07")
            .join("30")
            .join(format!("rollout-2026-07-30T00-00-00-{session_id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"timestamp\":\"2026-07-30T00:00:00Z\",\"payload\":{{\"id\":\"{session_id}\",\"session_id\":\"{session_id}\",\"cwd\":\"{cwd}\",\"model_provider\":\"openai\"}}}}\n{{\"type\":\"event_msg\",\"timestamp\":\"2026-07-30T00:00:01Z\",\"payload\":{{\"type\":\"message\",\"text\":\"{event}\"}}}}\n"
            ),
        )
        .unwrap();
        fs::write(
            home.join("session_index.jsonl"),
            format!(
                "{{\"id\":\"{session_id}\",\"thread_name\":\"{title}\",\"updated_at\":\"2026-07-30T00:00:01Z\"}}\n"
            ),
        )
        .unwrap();
        path
    }

    fn write_index(home: &Path, entries: &[(&str, &str, &str)]) {
        let lines = entries
            .iter()
            .map(|(session_id, title, updated_at)| {
                format!(
                    "{{\"id\":\"{session_id}\",\"thread_name\":\"{title}\",\"updated_at\":\"{updated_at}\"}}"
                )
            })
            .collect::<Vec<_>>();
        fs::write(
            home.join("session_index.jsonl"),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
    }

    fn create_threads_table(connection: &Connection) {
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    title TEXT NOT NULL,
                    sandbox_policy TEXT NOT NULL,
                    approval_mode TEXT NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
    }

    #[test]
    fn list_groups_sessions_by_project_and_filters_selected_project() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("project-list");
        let repository = repository(&root);
        std::env::set_var("HOME", root.join("home"));
        let default_home = root.join("home/.codex");
        let project_a = Uuid::new_v4().to_string();
        let project_b = Uuid::new_v4().to_string();
        rollout_with_cwd(
            &default_home,
            &project_a,
            "电检项目",
            "from-a",
            "/Users/test/electrica-inspection",
        );
        rollout_with_cwd(
            &default_home,
            &project_b,
            "Relay 项目",
            "from-b",
            "/Users/test/codex-reply",
        );
        write_index(
            &default_home,
            &[
                (&project_a, "电检项目", "2030-01-01T00:00:01Z"),
                (&project_b, "Relay 项目", "2030-01-01T00:00:10Z"),
            ],
        );
        let project_id = project_identity(Some("/Users/test/electrica-inspection")).id;

        let report = list_codex_history(
            &repository,
            &root,
            ListCodexHistoryInput {
                limit: Some(100),
                offset: Some(0),
                project_id: Some(project_id.clone()),
            },
        )
        .unwrap();

        assert_eq!(report.projects.len(), 2);
        assert!(report
            .projects
            .iter()
            .any(|project| project.name == "electrica-inspection"));
        assert_eq!(
            report.selected_project_id.as_deref(),
            Some(project_id.as_str())
        );
        assert_eq!(report.total_sessions, 1);
        assert_eq!(report.sessions[0].id, project_a);
        assert_eq!(report.sessions[0].project_name, "electrica-inspection");
    }

    #[test]
    fn list_groups_missing_cwd_into_uncategorized_project() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("project-uncategorized");
        let repository = repository(&root);
        std::env::set_var("HOME", root.join("home"));
        let default_home = root.join("home/.codex");
        let session_id = Uuid::new_v4().to_string();
        let path = default_home
            .join("sessions/2026/07/30")
            .join(format!("rollout-2026-07-30T00-00-00-{session_id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"timestamp\":\"2026-07-30T00:00:00Z\",\"payload\":{{\"id\":\"{session_id}\",\"model_provider\":\"openai\"}}}}\n"
            ),
        )
        .unwrap();
        write_index(
            &default_home,
            &[(&session_id, "无 cwd", "2030-01-01T00:00:01Z")],
        );

        let report =
            list_codex_history(&repository, &root, ListCodexHistoryInput::default()).unwrap();

        assert_eq!(
            report.selected_project_id.as_deref(),
            Some(UNCATEGORIZED_PROJECT_ID)
        );
        assert_eq!(report.projects[0].name, UNCATEGORIZED_PROJECT_NAME);
        assert_eq!(report.sessions[0].project_id, UNCATEGORIZED_PROJECT_ID);
    }

    #[test]
    fn delete_moves_history_to_trash_and_preserves_credentials() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("delete");
        let repository = repository(&root);
        std::env::set_var("HOME", root.join("home"));
        let default_home = root.join("home/.codex");
        fs::create_dir_all(&default_home).unwrap();
        fs::write(default_home.join("auth.json"), "auth").unwrap();
        fs::write(
            default_home.join("config.toml"),
            "model_provider = \"openai\"\n",
        )
        .unwrap();
        let session_id = Uuid::new_v4().to_string();
        let rollout_path = rollout(&default_home, &session_id, "删除测试", "delete-me");
        let sqlite = default_home.join("state_5.sqlite");
        let connection = Connection::open(&sqlite).unwrap();
        create_threads_table(&connection);
        connection
            .execute(
                "INSERT INTO threads
                 (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, archived)
                 VALUES (?1, ?2, 1785369600, 1785369601, 'local', 'openai', '/work', '删除测试', 'workspace-write', 'on-request', 0)",
                params![session_id, rollout_path.display().to_string()],
            )
            .unwrap();

        let report = delete_codex_history(
            &repository,
            &root,
            DeleteCodexHistoryInput {
                scope: "sessions".to_owned(),
                session_ids: vec![session_id.clone()],
                project_id: None,
                confirmed: true,
            },
        )
        .unwrap();

        assert_eq!(report.sessions_affected, 1);
        assert!(!rollout_path.exists());
        assert!(
            !fs::read_to_string(default_home.join("session_index.jsonl"))
                .unwrap()
                .contains(&session_id)
        );
        let remaining: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
        assert_eq!(
            fs::read_to_string(default_home.join("auth.json")).unwrap(),
            "auth"
        );
        assert_eq!(
            fs::read_to_string(default_home.join("config.toml")).unwrap(),
            "model_provider = \"openai\"\n"
        );
        assert!(root
            .join("session-history-trash")
            .read_dir()
            .unwrap()
            .next()
            .is_some());
    }

    #[test]
    fn export_writes_relay_zip_without_credentials() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("export");
        let repository = repository(&root);
        std::env::set_var("HOME", root.join("home"));
        let default_home = root.join("home/.codex");
        let session_id = Uuid::new_v4().to_string();
        rollout(&default_home, &session_id, "导出测试", "export-me");
        fs::write(default_home.join("auth.json"), "auth").unwrap();
        let destination = root.join("history.codex-history.zip");

        let report = export_codex_history(
            &repository,
            &root,
            ExportCodexHistoryInput {
                scope: "all".to_owned(),
                session_ids: Vec::new(),
                project_id: None,
                destination_path: destination.display().to_string(),
                confirmed: true,
            },
        )
        .unwrap();

        assert_eq!(report.sessions_exported, 1);
        let file = fs::File::open(&destination).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut manifest = String::new();
        archive
            .by_name("manifest.json")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        assert!(manifest.contains(HISTORY_EXPORT_FORMAT));
        assert!(archive
            .by_name(&format!("rollouts/{session_id}.jsonl"))
            .is_ok());
        for index in 0..archive.len() {
            let name = archive.by_index(index).unwrap().name().to_owned();
            assert!(!name.contains("auth.json"));
            assert!(!name.contains("config.toml"));
        }
    }

    #[test]
    fn import_relay_zip_roundtrips_into_default_home() {
        let _guard = env_lock().lock().unwrap();
        let export_root = temp_root("import-source");
        let export_repository = repository(&export_root);
        std::env::set_var("HOME", export_root.join("home"));
        let export_home = export_root.join("home/.codex");
        let session_id = Uuid::new_v4().to_string();
        rollout(&export_home, &session_id, "导入测试", "import-me");
        let destination = export_root.join("history.codex-history.zip");
        export_codex_history(
            &export_repository,
            &export_root,
            ExportCodexHistoryInput {
                scope: "all".to_owned(),
                session_ids: Vec::new(),
                project_id: None,
                destination_path: destination.display().to_string(),
                confirmed: true,
            },
        )
        .unwrap();

        let import_root = temp_root("import-target");
        let import_repository = repository(&import_root);
        std::env::set_var("HOME", import_root.join("home"));
        let report = import_codex_history(
            &import_repository,
            &import_root,
            ImportCodexHistoryInput {
                paths: vec![destination.display().to_string()],
                confirmed: true,
            },
        )
        .unwrap();

        assert_eq!(report.sessions_imported, 1);
        let imported_home = import_root.join("home/.codex");
        let imported_rollouts = rollout_paths(&imported_home, &mut Vec::new());
        assert_eq!(imported_rollouts.len(), 1);
        assert!(fs::read_to_string(&imported_rollouts[0])
            .unwrap()
            .contains("import-me"));
        assert!(
            fs::read_to_string(imported_home.join("session_index.jsonl"))
                .unwrap()
                .contains("导入测试")
        );
    }

    #[test]
    fn list_uses_lightweight_rollout_summary_without_reading_body() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("list-lightweight");
        let repository = repository(&root);
        std::env::set_var("HOME", root.join("home"));
        let default_home = root.join("home/.codex");
        let session_id = Uuid::new_v4().to_string();
        let path = default_home
            .join("sessions/2026/07/30")
            .join(format!("rollout-2026-07-30T00-00-00-{session_id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let meta = format!(
            "{{\"type\":\"session_meta\",\"timestamp\":\"2026-07-30T00:00:00Z\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"/work\",\"model_provider\":\"openai\"}}}}\n"
        );
        let mut bytes = meta.into_bytes();
        bytes.extend(std::iter::repeat_n(0xff, 2 * 1024 * 1024));
        fs::write(&path, bytes).unwrap();
        write_index(
            &default_home,
            &[(&session_id, "轻量列表", "2030-01-01T00:00:01Z")],
        );

        let report =
            list_codex_history(&repository, &root, ListCodexHistoryInput::default()).unwrap();

        assert_eq!(report.limit, DEFAULT_HISTORY_LIST_LIMIT);
        assert_eq!(report.offset, 0);
        assert_eq!(report.total_sessions, 1);
        assert_eq!(report.sessions[0].id, session_id);
        assert_eq!(report.sessions[0].title.as_deref(), Some("轻量列表"));
        assert_eq!(report.sessions[0].sources[0].event_count, 0);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn list_respects_limit_offset_and_sorts_by_index_update() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("list-page");
        let repository = repository(&root);
        std::env::set_var("HOME", root.join("home"));
        let default_home = root.join("home/.codex");
        let older = Uuid::new_v4().to_string();
        let newer = Uuid::new_v4().to_string();
        rollout(&default_home, &older, "旧会话", "older");
        rollout(&default_home, &newer, "新会话", "newer");
        write_index(
            &default_home,
            &[
                (&older, "旧会话", "2030-01-01T00:00:01Z"),
                (&newer, "新会话", "2030-01-01T00:00:10Z"),
            ],
        );

        let report = list_codex_history(
            &repository,
            &root,
            ListCodexHistoryInput {
                limit: Some(1),
                offset: Some(1),
                project_id: None,
            },
        )
        .unwrap();

        assert_eq!(report.limit, 1);
        assert_eq!(report.offset, 1);
        assert_eq!(report.total_sessions, 2);
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].id, older);
    }

    #[test]
    fn list_uses_index_title_and_sqlite_fallback_metadata() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("list-sqlite");
        let repository = repository(&root);
        std::env::set_var("HOME", root.join("home"));
        let default_home = root.join("home/.codex");
        let session_id = Uuid::new_v4().to_string();
        let path = default_home
            .join("sessions/2026/07/30")
            .join(format!("rollout-2026-07-30T00-00-00-{session_id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"timestamp\":\"2026-07-30T00:00:00Z\",\"payload\":{{\"id\":\"{session_id}\",\"model_provider\":\"openai\"}}}}\n"
            ),
        )
        .unwrap();
        write_index(
            &default_home,
            &[(&session_id, "Index 标题", "2030-01-01T00:00:01Z")],
        );
        let connection = Connection::open(default_home.join("state_5.sqlite")).unwrap();
        create_threads_table(&connection);
        connection
            .execute(
                "INSERT INTO threads
                 (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, archived)
                 VALUES (?1, ?2, 1893456000, 1893456010, 'local', 'openai', '/sqlite-work', 'SQLite 标题', 'workspace-write', 'on-request', 0)",
                params![session_id, path.display().to_string()],
            )
            .unwrap();

        let report =
            list_codex_history(&repository, &root, ListCodexHistoryInput::default()).unwrap();

        assert_eq!(report.sessions[0].title.as_deref(), Some("Index 标题"));
        assert_eq!(report.sessions[0].cwd.as_deref(), Some("/sqlite-work"));
        assert_eq!(report.sessions[0].updated_at_ms, 1_893_456_010_000);
    }

    #[test]
    fn sync_copies_missing_rollout_without_touching_auth_or_config() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("copy");
        let repository = repository(&root);
        insert_profile(&repository, "profile-a", "工作号");
        let default_home = root.join("home").join(".codex");
        std::env::set_var("HOME", root.join("home"));
        fs::create_dir_all(&default_home).unwrap();
        fs::write(default_home.join("auth.json"), "auth").unwrap();
        fs::write(
            default_home.join("config.toml"),
            "model_provider = \"openai\"\n",
        )
        .unwrap();
        let session_id = Uuid::new_v4().to_string();
        rollout(&default_home, &session_id, "测试会话", "from-default");

        let report = sync_codex_history(&repository, &root).unwrap();

        assert!(matches!(report.status.as_str(), "completed" | "warning"));
        assert!(report.files_written >= 2);
        assert_eq!(
            fs::read_to_string(default_home.join("auth.json")).unwrap(),
            "auth"
        );
        assert_eq!(
            fs::read_to_string(default_home.join("config.toml")).unwrap(),
            "model_provider = \"openai\"\n"
        );
        assert!(root
            .join("runtimes/profile-a/sessions/2026/07/30")
            .read_dir()
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(&session_id)));
    }

    #[test]
    fn sync_merges_divergent_rollout_events() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("merge");
        let repository = repository(&root);
        insert_profile(&repository, "profile-a", "工作号");
        std::env::set_var("HOME", root.join("home"));
        let default_home = root.join("home").join(".codex");
        let profile_home = root.join("runtimes/profile-a");
        let session_id = Uuid::new_v4().to_string();
        rollout(&default_home, &session_id, "测试会话", "from-default");
        rollout(&profile_home, &session_id, "测试会话", "from-profile");

        sync_codex_history(&repository, &root).unwrap();

        let target = fs::read_to_string(
            default_home
                .join("sessions/2026/07/30")
                .read_dir()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(target.contains("from-default"));
        assert!(target.contains("from-profile"));
        assert_eq!(target.matches("\"type\":\"session_meta\"").count(), 1);
    }

    #[test]
    fn sync_repairs_sqlite_thread_metadata() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("sqlite");
        let repository = repository(&root);
        insert_profile(&repository, "profile-a", "工作号");
        std::env::set_var("HOME", root.join("home"));
        let default_home = root.join("home/.codex");
        let profile_home = root.join("runtimes/profile-a");
        fs::create_dir_all(&profile_home).unwrap();
        fs::write(
            profile_home.join("config.toml"),
            "model_provider = \"codex_relay\"\n",
        )
        .unwrap();
        let session_id = Uuid::new_v4().to_string();
        rollout(&default_home, &session_id, "测试会话", "from-default");
        let sqlite = profile_home.join("state_5.sqlite");
        let connection = Connection::open(&sqlite).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    title TEXT NOT NULL,
                    sandbox_policy TEXT NOT NULL,
                    approval_mode TEXT NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();

        sync_codex_history(&repository, &root).unwrap();

        let provider: String = connection
            .query_row(
                "SELECT model_provider FROM threads WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "codex_relay");
    }

    #[test]
    fn sync_fails_before_overwrite_when_backup_root_is_blocked() {
        let _guard = env_lock().lock().unwrap();
        let root = temp_root("backup-failure");
        let repository = repository(&root);
        insert_profile(&repository, "profile-a", "工作号");
        std::env::set_var("HOME", root.join("home"));
        let default_home = root.join("home/.codex");
        let session_id = Uuid::new_v4().to_string();
        rollout(&default_home, &session_id, "测试会话", "from-default");
        fs::create_dir_all(root.join("runtimes/profile-a/sessions/2026/07/30")).unwrap();
        fs::write(
            root.join(format!(
                "runtimes/profile-a/sessions/2026/07/30/rollout-2026-07-30T00-00-00-{session_id}.jsonl"
            )),
            "different",
        )
        .unwrap();
        fs::write(root.join("session-history-backups"), "blocked").unwrap();

        let error = sync_codex_history(&repository, &root).unwrap_err();

        assert!(matches!(error, AppError::RuntimeUnavailable));
    }
}
