//! `duckdb_p2p` — the loadable DuckDB C-API extension surface (architecture §12).
//!
//! Phase 0 walking-skeleton surface. It is built as a loadable extension against
//! DuckDB's **stable C extension API** (so it loads via `LOAD 'duckdb_p2p'`
//! without linking the whole engine). It exposes table functions that prove the
//! extension loads and is wired to the workspace crates:
//!
//!  * `p2p_info()`   → protocol/version/build metadata (from `p2p-proto`).
//!  * `p2p_peers()`  → the bootstrap/seed peers from the resolved config
//!                     (`p2p-config`, honoring the `P2P_CONFIG` env var).
//!
//! The full distributed `p2p_query` / `p2p_share` / `p2p_join` surface drives the
//! async coordinator/worker in `p2p-node`; that path is exercised by the Rust
//! scenario suite (it needs live peers, which a single in-process `LOAD` cannot
//! provide). See `docs/ARCHITECTURE.md` and the scenario suite.

use std::error::Error;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};

use duckdb::core::{DataChunkHandle, Inserter, LogicalTypeHandle, LogicalTypeId};
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab};
use duckdb::{duckdb_entrypoint_c_api, Connection, Result};

/// Rows materialized at bind time; emitted in one chunk.
#[repr(C)]
struct Rows2 {
    rows: Vec<(String, String)>,
}

#[repr(C)]
struct OnceInit {
    done: AtomicBool,
}

/// `p2p_info()` → (key VARCHAR, value VARCHAR).
struct InfoVTab;

impl VTab for InfoVTab {
    type InitData = OnceInit;
    type BindData = Rows2;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("key", LogicalTypeHandle::from(LogicalTypeId::Varchar));
        bind.add_result_column("value", LogicalTypeHandle::from(LogicalTypeId::Varchar));
        let rows = vec![
            ("protocol_name".to_string(), p2p_proto::PROTOCOL_NAME.to_string()),
            (
                "protocol_version".to_string(),
                p2p_proto::PROTOCOL_VERSION.to_string(),
            ),
            (
                "min_supported_version".to_string(),
                p2p_proto::MIN_SUPPORTED_VERSION.to_string(),
            ),
            (
                "schema_version".to_string(),
                p2p_proto::SCHEMA_VERSION.to_string(),
            ),
            (
                "extension_version".to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            ),
            (
                "alpn".to_string(),
                String::from_utf8_lossy(&p2p_proto::current_alpn()).to_string(),
            ),
        ];
        Ok(Rows2 { rows })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        Ok(OnceInit {
            done: AtomicBool::new(false),
        })
    }

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        let init = func.get_init_data();
        let bind = func.get_bind_data();
        if init.done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        emit_two_columns(output, &bind.rows)?;
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// `p2p_peers()` → (kind VARCHAR, value VARCHAR) describing configured seeds.
struct PeersVTab;

impl VTab for PeersVTab {
    type InitData = OnceInit;
    type BindData = Rows2;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("kind", LogicalTypeHandle::from(LogicalTypeId::Varchar));
        bind.add_result_column("value", LogicalTypeHandle::from(LogicalTypeId::Varchar));

        // Resolve config (defaults <- file via P2P_CONFIG <- env). On error,
        // surface a single diagnostic row instead of failing the LOAD.
        let rows = match p2p_config::GridConfig::load(None) {
            Ok(cfg) => {
                let mut rows = vec![
                    ("discovery_mode".to_string(), format!("{:?}", cfg.discovery.mode)),
                    (
                        "candidate_sample_size".to_string(),
                        cfg.discovery.candidate_sample_size.to_string(),
                    ),
                ];
                for seed in &cfg.discovery.bootstrap {
                    rows.push(("bootstrap".to_string(), seed.clone()));
                }
                rows
            }
            Err(e) => vec![("config_error".to_string(), e.to_string())],
        };
        Ok(Rows2 { rows })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        Ok(OnceInit {
            done: AtomicBool::new(false),
        })
    }

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        let init = func.get_init_data();
        let bind = func.get_bind_data();
        if init.done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        emit_two_columns(output, &bind.rows)?;
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

/// Emit `rows` as two VARCHAR columns into a single output chunk.
fn emit_two_columns(output: &mut DataChunkHandle, rows: &[(String, String)]) -> Result<(), Box<dyn Error>> {
    {
        let col0 = output.flat_vector(0);
        for (i, (k, _)) in rows.iter().enumerate() {
            col0.insert(i, CString::new(k.as_str())?);
        }
    }
    {
        let col1 = output.flat_vector(1);
        for (i, (_, v)) in rows.iter().enumerate() {
            col1.insert(i, CString::new(v.as_str())?);
        }
    }
    output.set_len(rows.len());
    Ok(())
}

#[duckdb_entrypoint_c_api(ext_name = "duckdb_p2p", min_duckdb_version = "v1.0.0")]
pub fn duckdb_p2p_init(con: Connection) -> Result<(), Box<dyn Error>> {
    con.register_table_function::<InfoVTab>("p2p_info")?;
    con.register_table_function::<PeersVTab>("p2p_peers")?;
    Ok(())
}
