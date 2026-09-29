//! A small app that keeps notes in RocksDB.
//!
//! Run the command below. Then send `curl -X POST localhost:3000/notes/1 -d hello`
//! and open `http://localhost:3000/notes/1`.
//!
//! ```sh
//! cargo run --example notes
//! ```

use autumn_plugin_rocksdb::{RocksDb, RocksDbPlugin, RocksDbResultExt as _};
use autumn_web::prelude::*;

#[get("/notes/{id}")]
async fn read(db: RocksDb, Path(id): Path<u64>) -> AutumnResult<String> {
    let note = db.cf("notes").get(id.to_be_bytes()).await.or_http()?;
    let note = note.ok_or_else(|| AutumnError::not_found_msg("no note"))?;
    Ok(String::from_utf8_lossy(&note).into_owned())
}

#[post("/notes/{id}")]
async fn write(db: RocksDb, Path(id): Path<u64>, body: String) -> AutumnResult<&'static str> {
    db.cf("notes").put(id.to_be_bytes(), body).await.or_http()?;
    Ok("saved")
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(RocksDbPlugin::new().configure(|c| {
            c.path = "target/notes-rocksdb".into();
            c.column_families = vec!["notes".into()];
        }))
        .routes(routes![read, write])
        .run()
        .await;
}
