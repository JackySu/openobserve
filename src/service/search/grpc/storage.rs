// Copyright 2024 Zinc Labs Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::Arc;

use config::{
    get_config, is_local_disk_storage,
    meta::{
        bitvec::BitVec,
        inverted_index::reader::Contains,
        search::{ScanStats, SearchType, StorageType},
        stream::{FileKey, PartitionTimeLevel, StreamPartition, StreamType},
    },
    utils::{
        inverted_index::{
            convert_parquet_idx_file_name, create_index_reader_from_puffin_bytes, split_token,
        },
        schema_ext::SchemaExt,
    },
    INDEX_FIELD_NAME_FOR_ALL,
};
use fst::{automaton::Str, IntoStreamer, Streamer};
use futures::future::try_join_all;
use hashbrown::HashMap;
use infra::{
    cache::file_data,
    errors::{Error, ErrorCodes},
    schema::{unwrap_partition_time_level, unwrap_stream_settings},
    storage,
};
use itertools::Itertools;
use tokio::{sync::Semaphore, time::Duration};
use tracing::{info_span, Instrument};

use crate::service::{
    db, file_list,
    search::{
        datafusion::{exec, file_type::FileType},
        generate_search_schema, generate_select_start_search_schema,
        sql::{Sql, RE_SELECT_WILDCARD},
    },
};

type CachedFiles = (usize, usize);

/// search in remote object storage
#[tracing::instrument(name = "service:search:grpc:storage", skip_all, fields(org_id = sql.org_id, stream_name = sql.stream_name))]
pub async fn search(
    trace_id: &str,
    sql: Arc<Sql>,
    file_list: &[FileKey],
    stream_type: StreamType,
    work_group: &str,
    timeout: u64,
) -> super::SearchResult {
    let enter_span = tracing::span::Span::current();
    log::info!("[trace_id {trace_id}] search->storage: enter");
    let schema_latest = infra::schema::get(&sql.org_id, &sql.stream_name, stream_type)
        .await
        .map_err(|e| Error::ErrorCode(ErrorCodes::ServerInternalError(e.to_string())))?;
    // fetch all schema versions, group files by version
    let schema_versions = match infra::schema::get_versions(
        &sql.org_id,
        &sql.stream_name,
        stream_type,
        sql.meta.time_range,
    )
    .instrument(enter_span.clone())
    .await
    {
        Ok(versions) => versions,
        Err(err) => {
            log::error!("[trace_id {trace_id}] get schema error: {}", err);
            return Err(Error::ErrorCode(ErrorCodes::SearchStreamNotFound(
                sql.stream_name.clone(),
            )));
        }
    };
    log::info!(
        "[trace_id {trace_id}] search->storage: stream {}/{}/{}, get schema versions num {}",
        &sql.org_id,
        stream_type,
        &sql.stream_name,
        schema_versions.len()
    );
    if schema_versions.is_empty() {
        return Ok((vec![], ScanStats::new()));
    }
    let schema_latest_id = schema_versions.len() - 1;

    let stream_settings = unwrap_stream_settings(&schema_latest).unwrap_or_default();
    let partition_time_level =
        unwrap_partition_time_level(stream_settings.partition_time_level, stream_type);
    let defined_schema_fields = stream_settings.defined_schema_fields.unwrap_or_default();

    // get file list
    let mut files = match file_list.is_empty() {
        true => {
            get_file_list(
                trace_id,
                &sql,
                stream_type,
                partition_time_level,
                &stream_settings.partition_keys,
            )
            .instrument(enter_span.clone())
            .await?
        }
        false => file_list.to_vec(),
    };
    if files.is_empty() {
        return Ok((vec![], ScanStats::default()));
    }
    log::info!(
        "[trace_id {trace_id}] search->storage: stream {}/{}/{}, load file_list num {}",
        &sql.org_id,
        &stream_type,
        &sql.stream_name,
        files.len(),
    );

    // filter file_list if is an inverted index search
    let cfg = get_config();
    let use_inverted_index = cfg.common.inverted_index_enabled
        && !cfg.common.feature_query_without_index
        && sql.use_inverted_index
        && (sql.inverted_index_type == "fst" || sql.inverted_index_type == "both")
        && (!sql.fts_terms.is_empty() || !sql.index_terms.is_empty());
    if use_inverted_index {
        // TODO: need to log search on FST took time
        filter_file_list_by_inverted_index(trace_id, &mut files, &sql, stream_type).await?;
        log::info!(
            "[trace_id {trace_id}] search->storage: stream {}/{}/{}, FST inverted index reduced file_list num to {}",
            &sql.org_id,
            &stream_type,
            &sql.stream_name,
            files.len(),
        );
    }

    let mut files_group: HashMap<usize, Vec<FileKey>> =
        HashMap::with_capacity(schema_versions.len());
    let mut scan_stats = ScanStats::new();
    if !cfg.common.widening_schema_evolution || schema_versions.len() == 1 {
        let files = files.to_vec();
        scan_stats = match file_list::calculate_files_size(&files).await {
            Ok(size) => size,
            Err(err) => {
                log::error!("[trace_id {trace_id}] calculate files size error: {}", err);
                return Err(Error::ErrorCode(ErrorCodes::ServerInternalError(
                    "calculate files size error".to_string(),
                )));
            }
        };
        files_group.insert(schema_latest_id, files);
    } else {
        scan_stats.files = files.len() as i64;
        for file in files.iter() {
            // calculate scan size
            scan_stats.records += file.meta.records;
            scan_stats.original_size += file.meta.original_size;
            scan_stats.compressed_size += file.meta.compressed_size;
            // check schema version
            let schema_ver_id = match db::schema::filter_schema_version_id(
                &schema_versions,
                file.meta.min_ts,
                file.meta.max_ts,
            ) {
                Some(id) => id,
                None => {
                    log::error!(
                        "[trace_id {trace_id}] search->storage: file {} schema version not found, will use the latest schema, min_ts: {}, max_ts: {}",
                        &file.key,
                        file.meta.min_ts,
                        file.meta.max_ts
                    );
                    // HACK: use the latest version if not found in schema versions
                    schema_latest_id
                }
            };
            let group = files_group.entry(schema_ver_id).or_default();
            group.push(file.clone());
        }
    }

    log::info!(
        "[trace_id {trace_id}] search->storage: stream {}/{}/{}, load files {}, scan_size {}, compressed_size {}",
        &sql.org_id,
        &stream_type,
        &sql.stream_name,
        scan_stats.files,
        scan_stats.original_size,
        scan_stats.compressed_size
    );

    if cfg.common.memory_circuit_breaker_enable {
        super::check_memory_circuit_breaker(trace_id, &scan_stats)?;
    }

    // load files to local cache
    let (cache_type, deleted_files, (mem_cached_files, disk_cached_files)) = cache_files(
        trace_id,
        &files.iter().map(|f| f.key.as_ref()).collect_vec(),
        &scan_stats,
    )
    .instrument(enter_span.clone())
    .await?;
    if !deleted_files.is_empty() {
        // remove deleted files from files_group
        for (_, g_files) in files_group.iter_mut() {
            g_files.retain(|f| !deleted_files.contains(&f.key));
        }
    }
    scan_stats.querier_files = scan_stats.files;
    scan_stats.querier_memory_cached_files = mem_cached_files as i64;
    scan_stats.querier_disk_cached_files = disk_cached_files as i64;
    log::info!(
        "[trace_id {trace_id}] search->storage: stream {}/{}/{}, load files {}, memory cached {}, disk cached {}, download others into {:?} cache done",
        &sql.org_id,
        &stream_type,
        &sql.stream_name,
        scan_stats.querier_files,
        scan_stats.querier_memory_cached_files,
        scan_stats.querier_disk_cached_files,
        cache_type,
    );

    // set target partitions based on cache type
    let target_partitions = if cache_type == file_data::CacheType::None {
        cfg.limit.query_thread_num
    } else {
        cfg.limit.cpu_num
    };

    // construct latest schema map
    let mut schema_latest_map = HashMap::with_capacity(schema_latest.fields().len());
    for field in schema_latest.fields() {
        schema_latest_map.insert(field.name(), field);
    }
    let select_wildcard = RE_SELECT_WILDCARD.is_match(sql.origin_sql.as_str());

    let mut tasks = Vec::new();
    for (ver, files) in files_group {
        let schema = schema_versions[ver].clone();
        let schema_dt = schema
            .metadata()
            .get("start_dt")
            .cloned()
            .unwrap_or_default();
        let schema = schema.with_metadata(std::collections::HashMap::new());
        let schema = Arc::new(schema);
        let sql = sql.clone();
        let session = config::meta::search::Session {
            id: format!("{trace_id}-{ver}"),
            storage_type: StorageType::Memory,
            search_type: if !sql.meta.group_by.is_empty() {
                SearchType::Aggregation
            } else {
                SearchType::Normal
            },
            work_group: Some(work_group.to_string()),
            target_partitions,
        };

        // cacluate the diff between latest schema and group schema
        let (schema, diff_fields) = if select_wildcard {
            generate_select_start_search_schema(
                &sql,
                schema.clone(),
                &schema_latest_map,
                &defined_schema_fields,
            )?
        } else {
            generate_search_schema(&sql, schema.clone(), &schema_latest_map)?
        };

        let datafusion_span = info_span!(
            "service:search:grpc:storage:datafusion",
            org_id = sql.org_id,
            stream_name = sql.stream_name,
            stream_type = stream_type.to_string(),
        );

        #[cfg(feature = "enterprise")]
        let (abort_sender, abort_receiver) = tokio::sync::oneshot::channel();
        #[cfg(feature = "enterprise")]
        if crate::service::search::SEARCH_SERVER
            .insert_sender(trace_id, abort_sender)
            .await
            .is_err()
        {
            log::info!(
                "[trace_id {}] search->storage: search canceled before call search->storage",
                session.id
            );
            return Err(Error::Message(format!(
                "[trace_id {}] search->storage: search canceled before call search->storage",
                session.id
            )));
        }

        let task = tokio::task::spawn(
            async move {
                tokio::select! {
                    ret = exec::sql(
                        &session,
                        schema.clone(),
                        diff_fields,
                        &sql,
                        &files,
                        None,
                        FileType::PARQUET,
                    ) => {
                        match ret {
                            Ok(ret) => Ok(ret),
                            Err(err) => {
                                log::error!("[trace_id {}] search->storage: datafusion execute error: {}", session.id, err);
                                if err.to_string().contains("Invalid comparison operation") {
                                    // print the session_id, schema, sql, files
                                    let schema_version = format!("{}/{}/{}/{}", &sql.org_id, &stream_type, &sql.stream_name, schema_dt);
                                    let schema_fiels = schema.as_ref().simple_fields();
                                    let files = files.iter().map(|f| f.key.as_str()).collect::<Vec<_>>();
                                    log::error!("[trace_id {}] search->storage: schema and parquet mismatch, version: {}, schema: {:?}, files: {:?}",
                                        session.id, schema_version, schema_fiels, files);
                                }
                                Err(err)
                            }
                        }
                    },
                    _ = tokio::time::sleep(Duration::from_secs(timeout)) => {
                        log::error!("[trace_id {}] search->storage: search timeout", session.id);
                        Err(datafusion::error::DataFusionError::ResourcesExhausted(format!(
                            "[trace_id {}] search->storage: task timeout", session.id
                        )))
                    },
                    _ = async {
                        #[cfg(feature = "enterprise")]
                        let _ = abort_receiver.await;
                        #[cfg(not(feature = "enterprise"))]
                        futures::future::pending::<()>().await;
                    } => {
                        log::info!("[trace_id {}] search->storage: search canceled", session.id);
                        Err(datafusion::error::DataFusionError::Execution(format!(
                            "[trace_id {}] search->storage: task is cancel", session.id
                        )))
                    }
                }
            }
            .instrument(datafusion_span),
        );

        tasks.push(task);
    }

    let mut results = vec![];
    let task_results = try_join_all(tasks)
        .await
        .map_err(|e| Error::ErrorCode(ErrorCodes::ServerInternalError(e.to_string())))?;
    for ret in task_results {
        match ret {
            Ok(v) => results.extend(v),
            Err(err) => match err {
                datafusion::error::DataFusionError::ResourcesExhausted(e) => {
                    return Err(Error::ErrorCode(ErrorCodes::SearchTimeout(e)));
                }
                _ => return Err(err.into()),
            },
        };
    }

    Ok((results, scan_stats))
}

#[tracing::instrument(name = "service:search:grpc:storage:get_file_list", skip_all, fields(org_id = sql.org_id, stream_name = sql.stream_name))]
async fn get_file_list(
    trace_id: &str,
    sql: &Sql,
    stream_type: StreamType,
    time_level: PartitionTimeLevel,
    partition_keys: &[StreamPartition],
) -> Result<Vec<FileKey>, Error> {
    log::debug!(
        "[trace_id {trace_id}] search->storage: get file_list in grpc, stream {}/{}/{}, time_range {:?}",
        &sql.org_id,
        &stream_type,
        &sql.stream_name,
        &sql.meta.time_range
    );
    let (time_min, time_max) = sql.meta.time_range.unwrap();
    let file_list = match file_list::query(
        &sql.org_id,
        &sql.stream_name,
        stream_type,
        time_level,
        time_min,
        time_max,
        true,
    )
    .await
    {
        Ok(file_list) => file_list,
        Err(err) => {
            log::error!("[trace_id {trace_id}] get file list error: {}", err);
            return Err(Error::ErrorCode(ErrorCodes::ServerInternalError(
                "get file list error".to_string(),
            )));
        }
    };

    let mut files = Vec::with_capacity(file_list.len());
    for file in file_list {
        if sql
            .match_source(&file, false, false, stream_type, partition_keys)
            .await
        {
            files.push(file.to_owned());
        }
    }
    files.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(files)
}

#[tracing::instrument(name = "service:search:grpc:storage:cache_files", skip_all)]
async fn cache_files<'a>(
    trace_id: &str,
    files: &[&str],
    scan_stats: &ScanStats,
) -> Result<(file_data::CacheType, Vec<String>, CachedFiles), Error> {
    let cfg = get_config();
    let cache_type = if cfg.memory_cache.enabled
        && scan_stats.compressed_size < cfg.memory_cache.skip_size as i64
    {
        // if scan_compressed_size < 80% of total memory cache, use memory cache
        file_data::CacheType::Memory
    } else if !is_local_disk_storage()
        && cfg.disk_cache.enabled
        && scan_stats.compressed_size < cfg.disk_cache.skip_size as i64
    {
        // if scan_compressed_size < 80% of total disk cache, use disk cache
        file_data::CacheType::Disk
    } else {
        // no cache
        return Ok((file_data::CacheType::None, vec![], (0, 0)));
    };

    let mut mem_cached_files = 0;
    let mut disk_cached_files = 0;

    let mut tasks = Vec::new();
    let semaphore = std::sync::Arc::new(Semaphore::new(cfg.limit.query_thread_num));
    for file in files.iter() {
        let trace_id = trace_id.to_string();
        let file_name = file.to_string();
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let task: tokio::task::JoinHandle<(Option<String>, bool, bool)> = tokio::task::spawn(
            async move {
                let cfg = get_config();
                let ret = match cache_type {
                    file_data::CacheType::Memory => {
                        let mut disk_exists = false;
                        let mem_exists = file_data::memory::exist(&file_name).await;
                        if !mem_exists && !cfg.memory_cache.skip_disk_check {
                            // when skip_disk_check = false, need to check disk cache
                            disk_exists = file_data::disk::exist(&file_name).await;
                        }
                        if !mem_exists && (cfg.memory_cache.skip_disk_check || !disk_exists) {
                            (
                                file_data::memory::download(&trace_id, &file_name)
                                    .await
                                    .err(),
                                false,
                                false,
                            )
                        } else {
                            (None, mem_exists, disk_exists)
                        }
                    }
                    file_data::CacheType::Disk => {
                        if !file_data::disk::exist(&file_name).await {
                            (
                                file_data::disk::download(&trace_id, &file_name).await.err(),
                                false,
                                false,
                            )
                        } else {
                            (None, false, true)
                        }
                    }
                    _ => (None, false, false),
                };
                // TODO: we can't delete index file
                let file_name = if let Some(e) = ret.0 {
                    if e.to_string().to_lowercase().contains("not found")
                        || e.to_string().to_lowercase().contains("data size is zero")
                    {
                        // delete file from file list
                        log::warn!("found invalid file: {}", file_name);
                        if let Err(e) = file_list::delete_parquet_file(&file_name, true).await {
                            log::error!(
                                "[trace_id {trace_id}] search->storage: delete from file_list err: {}",
                                e
                            );
                        }
                        Some(file_name)
                    } else {
                        log::error!(
                            "[trace_id {trace_id}] search->storage: download file to cache err: {}",
                            e
                        );
                        None
                    }
                } else {
                    None
                };
                drop(permit);
                (file_name, ret.1, ret.2)
            },
        );
        tasks.push(task);
    }

    let mut delete_files = Vec::new();
    for task in tasks {
        match task.await {
            Ok((file, mem_exists, disk_exists)) => {
                if mem_exists {
                    mem_cached_files += 1;
                } else if disk_exists {
                    disk_cached_files += 1;
                }
                if let Some(file) = file {
                    delete_files.push(file);
                }
            }
            Err(e) => {
                log::error!(
                    "[trace_id {trace_id}] search->storage: load file task err: {}",
                    e
                );
            }
        }
    }

    Ok((
        cache_type,
        delete_files,
        (mem_cached_files, disk_cached_files),
    ))
}

/// Filter file list using inverted index
/// This function will load the index file corresponding to each file in the file list.
/// FSTs in those files are used to match the incoming query in `SearchRequest`.
/// If the query does not match any FST in the index file, the file will be filtered out.
/// If the query does match then the segment IDs for the file will be updated.
/// If the query not find corresponding index file, the file will *not* be filtered out.
async fn filter_file_list_by_inverted_index(
    trace_id: &str,
    file_list: &mut Vec<FileKey>,
    sql: &Sql,
    stream_type: StreamType,
) -> Result<(), Error> {
    // Cache the corresponding Index files
    let mut scan_stats = ScanStats::new();
    let index_file_names = file_list
        .iter()
        .filter_map(|f| convert_parquet_idx_file_name(&f.key))
        .collect_vec();
    let index_file_names = index_file_names.iter().map(|f| f.as_str()).collect_vec();
    let (cache_type, _, (mem_cached_files, disk_cached_files)) =
        cache_files(trace_id, index_file_names.as_ref(), &scan_stats).await?;

    scan_stats.querier_memory_cached_files = mem_cached_files as i64;
    scan_stats.querier_disk_cached_files = disk_cached_files as i64;
    log::info!(
        "[trace_id {trace_id}] search->storage: stream {}/{}/{}, load puffin index files {}, memory cached {}, disk cached {}, download others into {:?} cache done",
        &sql.org_id,
        &stream_type,
        &sql.stream_name,
        scan_stats.querier_files,
        scan_stats.querier_memory_cached_files,
        scan_stats.querier_disk_cached_files,
        cache_type,
    );

    let full_text_terms = Arc::new(
        sql.fts_terms
            .iter()
            .map(|term| {
                let tokens = split_token(
                    term,
                    &config::get_config().common.inverted_index_split_chars,
                );
                tokens
                    .into_iter()
                    .max_by_key(|t| t.len())
                    .unwrap_or_default()
            })
            .collect_vec(),
    );
    let index_terms = Arc::new(sql.index_terms.clone());
    // we can be iterating over a lot of files
    // TODO: add a limit to the number of files we can process in parallel
    let mut tasks = Vec::new();
    for file in file_list.iter() {
        let full_text_term_clone = full_text_terms.clone();
        let index_terms_clone = index_terms.clone();
        let file_name = file.key.clone();
        // Spawn a task for each file, wherein full text search and
        // index search queries are executed
        let task = tokio::task::spawn(async move {
            inverted_index_search_in_file(&file_name, full_text_term_clone, index_terms_clone).await
        });
        tasks.push(task)
    }

    for result in try_join_all(tasks)
        .await
        .map_err(|e| Error::ErrorCode(ErrorCodes::ServerInternalError(e.to_string())))?
    {
        // Each result corresponds to a file in the file list
        match result {
            Ok((file_name, bitvec)) => {
                if let Some(res) = bitvec {
                    // Replace the segment IDs in the existing `FileKey` with the new found segments
                    // TODO: need a hashmap to avoid this O(n) operation
                    file_list
                        .iter_mut()
                        .find(|f| f.key == file_name)
                        // File should exist in the file list
                        .unwrap()
                        .segment_ids = Some(res.clone().into_vec());
                    log::info!(
                        "Final bitmap for fts_terms {:?} and index_terms: {:?} is {:?}",
                        full_text_terms,
                        index_terms,
                        res.iter_ones().collect_vec()
                    );
                } else {
                    // TODO: some parquet file maybe have no index file, need to filter out
                    log::info!(
                        "[trace_id {trace_id}] search->storage: no match found in index for file {}",
                        file_name
                    );
                    file_list.retain(|f| f.key != file_name)
                }
            }
            Err(e) => {
                log::warn!(
                    "[trace_id {trace_id}] search->storage: error filtering file via FST index. Keep file to search. error: {}",
                    e.to_string()
                );
                continue;
            }
        }
    }
    Ok(())
}

async fn inverted_index_search_in_file(
    parquet_file_name: &str,
    fts_terms: Arc<Vec<String>>,
    index_terms: Arc<Vec<(String, Vec<String>)>>,
) -> anyhow::Result<(String, Option<BitVec>)> {
    let Some(index_file_name) = convert_parquet_idx_file_name(parquet_file_name) else {
        return Err(anyhow::anyhow!(
            "Unable to convert parquet file name {} to index file name",
            parquet_file_name
        ));
    };
    // TODO: already cached in cache_files, should not download again
    // first get from meory cache
    // second get from disk cache
    // at the end, still not data, get from storage
    let compressed_index_blob = match storage::get(&index_file_name).await {
        Err(e) => {
            log::warn!(
                "Unable to load corresponding FST index file for parquet file {}, err: {}",
                parquet_file_name,
                e
            );
            return Err(e);
        }
        Ok(bytes) => bytes,
    };

    let mut index_reader =
        create_index_reader_from_puffin_bytes(compressed_index_blob.to_vec()).await?;
    let file_meta = index_reader.metadata().await?;

    let mut res = BitVec::new();
    // filter through full text terms
    if let Some(column_index_meta) = &file_meta.metas.get(INDEX_FIELD_NAME_FOR_ALL) {
        // TODO: still have no min_value, max_value
        // max_len: 5, i want search: taiming
        let valid_terms = fts_terms
            .iter()
            .filter(|term| term.len() <= column_index_meta.max_len)
            .collect::<Vec<_>>();
        if !valid_terms.is_empty() {
            let fst_offset =
                column_index_meta.base_offset + column_index_meta.relative_fst_offset as u64;
            let fst_size = column_index_meta.fst_size;
            match index_reader.fst(fst_offset, fst_size).await {
                Err(e) => {
                    log::warn!(
                        "Error loading FST map from index file {} for column {} with error {}. Keep the file",
                        index_file_name,
                        INDEX_FIELD_NAME_FOR_ALL,
                        e.to_string()
                    );
                }
                Ok(fst_map) => {
                    // construct automatons for multiple full text search terms
                    let matchers = valid_terms
                        .iter()
                        .map(|term| Contains::new(term))
                        .collect::<Vec<Contains>>();

                    for matcher in matchers {
                        // Stream for matched keys and their bitmap offsets
                        let mut stream = fst_map.search(matcher).into_stream();
                        // We do not care about the key at this point, only the offset
                        while let Some((_, value)) = stream.next() {
                            let bitmap = index_reader.get_bitmap(column_index_meta, value).await?;

                            // Resize if the res map is smaller than the bitmap
                            if res.len() < bitmap.len() {
                                res.resize(bitmap.len(), false);
                            }
                            // bitwise OR to combine the bitmaps of all the terms
                            res |= bitmap;
                        }
                    }
                }
            };
        }
    }

    if !index_terms.is_empty() {
        for (col, index_terms) in index_terms.iter() {
            if let Some(column_index_meta) = file_meta.metas.get(col) {
                // TODO: still have no min_value, max_value
                let valid_terms = index_terms
                    .iter()
                    .filter(|term| {
                        term.len() >= column_index_meta.min_len
                            && term.len() <= column_index_meta.max_len
                    })
                    .collect::<Vec<_>>();
                if !valid_terms.is_empty() {
                    let fst_offset = column_index_meta.base_offset
                        + column_index_meta.relative_fst_offset as u64;
                    let fst_size = column_index_meta.fst_size;
                    match index_reader.fst(fst_offset, fst_size).await {
                        Err(e) => {
                            log::warn!(
                                "Error loading FST map from index file {} for column {} with error {}. Keep the file",
                                index_file_name,
                                col,
                                e.to_string()
                            );
                        }
                        Ok(fst_map) => {
                            // construct automatons for multiple full text search terms
                            let matchers = valid_terms
                                .iter()
                                .map(|term| Str::new(term))
                                .collect::<Vec<Str>>();

                            for matcher in matchers {
                                // Stream for matched keys and their bitmap offsets
                                let mut stream = fst_map.search(matcher).into_stream();
                                // We do not care about the key at this point, only the offset
                                while let Some((_, value)) = stream.next() {
                                    let bitmap =
                                        index_reader.get_bitmap(column_index_meta, value).await?;

                                    // Resize if the res map is smaller than the bitmap
                                    if res.len() < bitmap.len() {
                                        res.resize(bitmap.len(), false);
                                    }
                                    // here we are doing bitwise OR to combine the bitmaps of all
                                    // the terms
                                    res |= bitmap;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(if res.is_empty() {
        (parquet_file_name.into(), None) // no match -> skip the file in search
    } else {
        (parquet_file_name.into(), Some(res)) // match -> take the file in search
    })
}
