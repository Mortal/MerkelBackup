use std::fs;
use std::io::Read;
use std::os::linux::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;

use crypto::blake2b::Blake2b;
use crypto::digest::Digest;
use crypto::symmetriccipher::SynchronousStreamCipher;
use pbr::ProgressBar;
use rand::Rng;
use rusqlite::{params, Connection, Statement, NO_PARAMS};

use crate::shared::{check_response, retry, Config, EType, Error, Secrets};

const CHUNK_SIZE: u64 = 64 * 1024 * 1024;

struct State<'a> {
    secrets: Secrets,
    config: Config,
    client: reqwest::Client,
    scan: bool,
    transfer_bytes: u64,
    progress: Option<ProgressBar<std::io::Stdout>>,
    last_delete: i64,
    has_remote_stmt: Statement<'a>,
    update_remote_stmt: Statement<'a>,
    get_chunks_stmt: Statement<'a>,
    update_chunks_stmt: Statement<'a>,
    rng: rand::rngs::OsRng,
}

#[derive(PartialEq)]
enum HasChunkResult {
    YesCached,
    Yes,
    No,
}

fn has_chunk(chunk: &str, state: &mut State, size: Option<usize>) -> Result<HasChunkResult, Error> {
    let cnt: i64 = state
        .has_remote_stmt
        .query(params![chunk, state.last_delete])?
        .next()?
        .ok_or(Error::MissingRow())?
        .get(0)?;
    if cnt == 1 {
        return Ok(HasChunkResult::YesCached);
    }

    // For small chunks it is quicker to just reupload
    if let Some(size) = size {
        if size < 1024 * 16 {
            return Ok(HasChunkResult::No);
        }
    }

    let url = format!(
        "{}/chunks/{}/{}",
        &state.config.server,
        hex::encode(&state.secrets.bucket),
        &chunk
    );
    let res = retry(&mut || {
        state
            .client
            .head(&url[..])
            .basic_auth(&state.config.user, Some(&state.config.password))
            .send()
    })?;
    match res.status() {
        reqwest::StatusCode::OK => Ok(HasChunkResult::Yes),
        reqwest::StatusCode::NOT_FOUND => Ok(HasChunkResult::No),
        code => Err(Error::HttpStatus(code)),
    }
}

fn push_chunk(content: &[u8], state: &mut State) -> Result<String, Error> {
    let now = std::time::Instant::now();
    let mut hasher = Blake2b::new(256 / 8);
    hasher.input(&state.secrets.seed);
    hasher.input(content);
    let hash = hasher.result_str().to_string();
    let t0 = now.elapsed().as_millis();
    let hc = has_chunk(&hash, state, Some(content.len()))?;
    let t1 = now.elapsed().as_millis();
    let mut t2 = t1;
    if hc == HasChunkResult::No {
        let url = format!(
            "{}/chunks/{}/{}",
            &state.config.server,
            hex::encode(&state.secrets.bucket),
            &hash
        );

        let mut crypted = Vec::new();
        crypted.resize(content.len() + 12, 0);
        state.rng.fill(&mut crypted[..12]);

        crypto::chacha20::ChaCha20::new(&state.secrets.key, &crypted[..12])
            .process(content, &mut crypted[12..]);
        t2 = now.elapsed().as_millis();

        let res = retry(&mut || {
            state
                .client
                .put(&url[..])
                .basic_auth(&state.config.user, Some(&state.config.password))
                .body(reqwest::Body::from(crypted.clone()))
                .send()
        })?;
        match res.status() {
            reqwest::StatusCode::OK => (),
            reqwest::StatusCode::CONFLICT => {
                debug!("Conflict in upload");
            }
            code => return Err(Error::HttpStatus(code)),
        }
    }
    let t3 = now.elapsed().as_millis();
    if hc != HasChunkResult::YesCached {
        state.update_remote_stmt.execute(params![hash])?;
    }
    if let Some(p) = &mut state.progress {
        p.add(content.len() as u64);
    }
    let t4 = now.elapsed().as_millis();
    debug!(
        "Put chunk; chunk: {}, size: {}, hash: {}, head: {}, crypt: {} put: {}, insert: {}",
        hash,
        content.len(),
        t0,
        t1 - t0,
        t2 - t1,
        t3 - t2,
        t4 - t3
    );
    Ok(hash)
}

fn backup_file(path: &Path, size: u64, mtime: u64, state: &mut State) -> Result<String, Error> {
    let path_str = path
        .to_str()
        .ok_or_else(|| Error::BadPath(path.to_path_buf()))?;
    if let Some(p) = &mut state.progress {
        let start = i64::max(0, path_str.len() as i64 - 40) as usize;
        p.message(&format!("{} ", &path_str[start..]));
    }

    // IF the file is empty we just do nothing
    if size == 0 {
        return Ok("empty".to_string());
    }

    // Check if we have allready checked the file once
    if !state.config.recheck {
        let chunks: Option<String> = {
            let mut rows =
                state
                    .get_chunks_stmt
                    .query(params![path_str, size as i64, mtime as i64])?;
            match rows.next()? {
                Some(row) => row.get(0)?,
                None => None,
            }
        };
        if let Some(chunks) = chunks {
            let mut good = true;
            for chunk in chunks.split(',') {
                if has_chunk(chunk, state, None)? == HasChunkResult::No {
                    good = false;
                    break;
                }
            }
            if good {
                return Ok(chunks);
            }
        }
    }

    if state.scan {
        state.transfer_bytes += size;
        return Ok("_".repeat((65 * (size + CHUNK_SIZE - 1) / CHUNK_SIZE - 1) as usize));
    }

    // Open the file and read each chunk
    let mut file = fs::File::open(&path)?;

    let mut buffer: Vec<u8> = Vec::new();
    buffer.resize(u64::min(size, CHUNK_SIZE) as usize, 0);
    let mut chunks = "".to_string();
    loop {
        let mut used = 0;
        while used < buffer.len() {
            let w = file.read(&mut buffer[used..])?;
            if w == 0 {
                break;
            }
            used += w;
        }
        if used == 0 {
            break;
        }

        if !chunks.is_empty() {
            chunks.push_str(&",");
        }
        chunks.push_str(&push_chunk(&buffer[..used], state)?);

        if used != buffer.len() {
            break;
        }
    }

    //TODO check if the mtime has changed while we where pushing

    state.update_chunks_stmt.execute(params![
        &path.to_str().unwrap(),
        size as i64,
        mtime as i64,
        &chunks
    ])?;
    Ok(chunks)
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DirEnt {
    name: String,
    etype: EType,
    content: String,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: i64,
    atime: i64,
    ctime: i64,
}

fn push_ents(mut entries: Vec<DirEnt>, state: &mut State) -> Result<(String, u64), Error> {
    entries.sort();
    let mut ans = "".to_string();
    for ent in entries {
        if !ans.is_empty() {
            ans.push('\0');
            ans.push('\0');
        }
        ans.push_str(&format!(
            "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            ent.name,
            ent.etype,
            ent.size,
            ent.content,
            ent.mode,
            ent.uid,
            ent.gid,
            ent.mtime,
            ent.atime,
            ent.ctime,
        ));
    }
    Ok((
        push_chunk(ans.as_bytes(), state)?,
        ans.as_bytes().len() as u64,
    ))
}

fn bytes_ents(entries: Vec<DirEnt>) -> u64 {
    let mut ans = 0;
    for ent in entries {
        if ans != 0 {
            ans += 1
        }
        ans += ent.name.len() + 25 + ent.content.len()
    }
    ans as u64
}

fn backup_folder(dir: &Path, state: &mut State) -> Result<(String, u64), Error> {
    let raw_entries = fs::read_dir(dir)?;
    let mut entries: Vec<DirEnt> = Vec::new();
    for entry in raw_entries {
        let path = entry?.path();
        let md = fs::symlink_metadata(&path)?;
        let filename = path
            .file_name()
            .ok_or_else(|| Error::BadPath(path.to_path_buf()))?
            .to_str()
            .ok_or_else(|| Error::BadPath(path.to_path_buf()))?;
        if filename.contains('\0') {
            return Err(Error::BadPath(path.to_path_buf()));
        }
        let ft = md.file_type();
        let mode = md.st_mode() & 0xFFF;
        if ft.is_dir() {
            let (content, size) = backup_folder(&path, state)?;
            entries.push(DirEnt {
                name: filename.to_string(),
                etype: EType::Dir,
                content,
                size,
                mode,
                uid: md.st_uid(),
                gid: md.st_gid(),
                atime: md.st_atime(),
                mtime: md.st_mtime(),
                ctime: md.st_ctime(),
            });
        } else if ft.is_file() {
            let mtime = md
                .modified()?
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            entries.push(DirEnt {
                name: filename.to_string(),
                etype: EType::File,
                content: backup_file(&path, md.len(), mtime, state)?,
                size: md.len(),
                mode,
                uid: md.st_uid(),
                gid: md.st_gid(),
                atime: md.st_atime(),
                mtime: md.st_mtime(),
                ctime: md.st_ctime(),
            });
        } else if ft.is_symlink() {
            let link = fs::read_link(&path)?;
            entries.push(DirEnt {
                name: filename.to_string(),
                etype: EType::Link,
                content: link
                    .to_str()
                    .ok_or_else(|| Error::BadPath(link.to_path_buf()))?
                    .to_string(),
                size: 0,
                mode,
                uid: md.st_uid(),
                gid: md.st_gid(),
                atime: md.st_atime(),
                mtime: md.st_mtime(),
                ctime: md.st_ctime(),
            });
        }
    }

    if state.scan {
        let size = bytes_ents(entries);
        state.transfer_bytes += size;
        Ok(("00000000000000000000000000000000".to_string(), size))
    } else {
        push_ents(entries, state)
    }
}

pub fn run(config: Config, secrets: Secrets) -> Result<(), Error> {
    let conn = Connection::open(&config.cache_db)?;

    conn.pragma_update(None, "journal_mode", &"WAL".to_string())?;

    conn.execute(
        "create table if not exists files (
            path text not null unique,
            size integer not null,
            mtime integer not null,
            chunks text not null
        )",
        NO_PARAMS,
    )?;

    conn.execute(
        "create table if not exists remote (
            chunk text not null unique,
            time integer not null
        )",
        NO_PARAMS,
    )?;

    let mut state = State {
        secrets,
        config,
        client: reqwest::Client::new(),
        scan: true,
        transfer_bytes: 0,
        progress: None,
        last_delete: 0,
        has_remote_stmt: conn
            .prepare("SELECT count(*) FROM remote WHERE chunk = ? AND time > ?")?,
        update_remote_stmt: conn
            .prepare("REPLACE INTO remote VALUES (?, strftime('%s', 'now'))")?,
        get_chunks_stmt: conn
            .prepare("SELECT chunks FROM files WHERE path = ? AND size = ? AND mtime = ?")?,
        update_chunks_stmt: conn
            .prepare("REPLACE INTO files (path, size, mtime, chunks) VALUES (?, ?, ?, ?)")?,
        rng: rand::rngs::OsRng::new().map_err(|_| Error::Msg("Unable to open rng"))?,
    };

    {
        let url = format!(
            "{}/status/{}",
            &state.config.server,
            hex::encode(&state.secrets.bucket)
        );

        state.last_delete = check_response(&mut || {
            state
                .client
                .get(&url[..])
                .basic_auth(&state.config.user, Some(&state.config.password))
                .send()
        })?
        .text()?
        .parse()?
    }

    let dirs = state.config.backup_dirs.clone();
    for dir in dirs.iter() {
        let path = Path::new(dir);
        if !path.is_dir() {
            info!("Skipping {}", &dir);
            continue;
        }
        info!("Scanning {}", &dir);
        backup_folder(path, &mut state)?;
    }

    if state.config.verbosity >= log::LevelFilter::Info {
        state.progress = Some({
            let mut p = ProgressBar::new(state.transfer_bytes);
            p.set_max_refresh_rate(Some(Duration::from_millis(500)));
            p.set_units(pbr::Units::Bytes);
            p.set_width(Some(140));
            p
        });
    }

    let mut entries: Vec<DirEnt> = Vec::new();
    state.scan = false;
    for dir in dirs.iter() {
        let path = Path::new(dir);
        if !path.is_dir() {
            info!("Skipping {}", &dir);
            continue;
        }
        info!("Backing up {}", &dir);

        let md = fs::metadata(&path)?;

        let (content, size) = backup_folder(path, &mut state)?;
        entries.push(DirEnt {
            name: dir.to_string(),
            etype: EType::Dir,
            content,
            size,
            mode: md.st_mode() & 0xFFF,
            uid: md.st_uid(),
            gid: md.st_gid(),
            atime: md.st_atime(),
            mtime: md.st_mtime(),
            ctime: md.st_ctime(),
        });
    }

    info!("Storing root");
    let (root, _) = push_ents(entries, &mut state)?;

    let url = format!(
        "{}/roots/{}/{}",
        &state.config.server,
        hex::encode(&state.secrets.bucket),
        &state.config.hostname
    );

    check_response(&mut || {
        state
            .client
            .put(&url[..])
            .basic_auth(&state.config.user, Some(&state.config.password))
            .body(root.clone())
            .send()
    })?;
    Ok(())
}
