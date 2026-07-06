//! gfx_tool — extract and re-inject Scaleform `.gfx`/`.swf` movies in Mercenaries 2.
//!
//! The Flash UI lives in `scaleform_*.block` blocks inside `vz.wad`. Each block is
//! `[u32 count][count x 16-byte entry][containers]`; a scaleform container is a UCFX
//! wrapper whose payload is a Scaleform movie (magic GFX/CFX = Scaleform, FWS/CWS =
//! raw/zlib Adobe SWF). This tool decompresses a block, locates that movie, and can
//! rebuild a `vz-patch.wad` overlay with a modified movie spliced back in.
//!
//! v1: `inspect` only — characterizes a real block so extract/build can be written
//! against ground truth rather than assumptions.

use std::fs::File;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use mercs2_formats::aset_type_ids::type_id_for_type_hash;
use mercs2_formats::crc32::crc32_mercs2;
use mercs2_formats::ffcs::{find_chunk, load_ffcs_archive, read_u32_le};
use mercs2_formats::hash::pandemic_hash_m2;
use mercs2_formats::patch_wad::{
    build_patch_wad_multi, merge_patch_wads, AsetEntry, PatchBlock, FFCS_CERT_BLOB,
};
use mercs2_formats::sges::{compress_sges, decompress_block};
use mercs2_formats::ucfx::{extract_data_chunk, walk_decompressed_block, BlockTableEntry};

const MOVIE_TYPE_ID: u32 = 23; // Scaleform movie ASET type (type_hash 0xFE0E8320)
const PAGE: usize = 0x8000;

/// Scaleform GFX and Adobe SWF file magics (3-byte): GFX/CFX = Scaleform
/// (raw/zlib), FWS/CWS = Adobe SWF (raw/zlib).
const MAGICS: [&[u8; 3]; 4] = [b"GFX", b"CFX", b"FWS", b"CWS"];

/// Find a *validated* movie header: MAGIC(3) + version(u8) + FileLength(u32 LE),
/// with a sane version (1..=20) and length (>=8, <64 MiB). The validation is what
/// rejects the false "CFX" inside the container's own "UCFX" magic (version byte
/// there is 0x50, out of range).
fn find_movie(data: &[u8]) -> Option<(usize, String, u8, u32)> {
    if data.len() < 8 {
        return None;
    }
    for i in 0..=data.len() - 8 {
        for m in MAGICS.iter() {
            if &data[i..i + 3] == &m[..] {
                let ver = data[i + 3];
                let len = u32::from_le_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]);
                if (1..=20).contains(&ver) && len >= 8 && (len as usize) < 64 * 1024 * 1024 {
                    return Some((i, String::from_utf8_lossy(&m[..]).into_owned(), ver, len));
                }
            }
        }
    }
    None
}

fn hex(data: &[u8], n: usize) -> String {
    data.iter()
        .take(n)
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Parser)]
#[command(name = "gfx_tool", about = "Extract / re-inject Scaleform .gfx movies in Mercs2 vz.wad")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Characterize a scaleform block: entries, container layout, movie magic offset, CSUM.
    Inspect {
        #[arg(long)]
        wad: PathBuf,
        /// Target block by index (from `cube_mod --list` / an inspect run).
        #[arg(long)]
        block_index: Option<usize>,
        /// Target block by path substring, e.g. "scaleform_genericbackground".
        #[arg(long)]
        block_name: Option<String>,
    },
    /// Resolve asset name(s) to pandemic_hash_m2 and locate them in the WAD's ASET.
    Find {
        #[arg(long)]
        wad: PathBuf,
        /// Asset name(s), e.g. "minimap.gfx" "shell.gfx".
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Extract a movie's `.gfx`/`.swf` file from a WAD to disk.
    Extract {
        #[arg(long)]
        wad: PathBuf,
        /// Movie asset name (e.g. "minimap"). Resolved via pandemic_hash_m2 + ASET.
        #[arg(long)]
        name: String,
        /// Output movie file.
        #[arg(long)]
        out: PathBuf,
    },
    /// Build a vz-patch.wad overriding one movie with a modified `.gfx`/`.swf`.
    Build {
        #[arg(long)]
        wad: PathBuf,
        /// Movie asset name to override (e.g. "minimap").
        #[arg(long)]
        name: String,
        /// The modified movie file to inject.
        #[arg(long)]
        movie: PathBuf,
        /// Output patch WAD.
        #[arg(long)]
        out: PathBuf,
        /// Merge into (replace-by-path within) this existing vz-patch.wad instead of a fresh one.
        #[arg(long)]
        merge: Option<PathBuf>,
    },
}

/// Locate a movie by name: (block_index, name_hash, type_hash, field_c, secondary_ref,
/// type_id, original_container_bytes).
struct MovieLoc {
    block_index: usize,
    name_hash: u32,
    type_hash: u32,
    field_c: u32,
    secondary_ref: u32,
    type_id: u32,
    container: Vec<u8>,
    block_path: String,
}

fn locate_movie(file: &mut File, archive: &mercs2_formats::ffcs::FfcsArchive, name: &str) -> Result<MovieLoc, String> {
    let name_hash = pandemic_hash_m2(name);
    let aset = archive
        .aset
        .iter()
        .find(|e| e.asset_hash == name_hash)
        .ok_or_else(|| format!("'{name}' (0x{name_hash:08X}) not in this WAD's ASET"))?;
    let block_index = aset.block_index() as usize;
    let secondary_ref = aset.secondary_ref;
    let type_id = aset.type_id;
    if type_id != MOVIE_TYPE_ID {
        eprintln!("warning: '{name}' type_id={type_id}, expected {MOVIE_TYPE_ID} (movie) — continuing anyway");
    }
    let block = decompress_block(file, &archive.indx, block_index as u16)
        .map_err(|e| format!("decompress block {block_index}: {e}"))?;
    let (parsed, _) = walk_decompressed_block(&block, "");
    let idx = parsed
        .entries
        .iter()
        .position(|e: &BlockTableEntry| e.name_hash == name_hash)
        .ok_or_else(|| format!("0x{name_hash:08X} not found in block {block_index} entry table"))?;
    let entry = &parsed.entries[idx];
    Ok(MovieLoc {
        block_index,
        name_hash,
        type_hash: entry.type_hash,
        field_c: entry.field_c,
        secondary_ref,
        type_id,
        container: parsed.containers[idx].clone(),
        block_path: archive.paths.get(block_index).cloned().unwrap_or_default(),
    })
}

/// The stored movie file (the `data` chunk) inside a movie container.
fn container_movie(container: &[u8]) -> Result<Vec<u8>, String> {
    extract_data_chunk(container).ok_or_else(|| "container has no `data` chunk (not a movie?)".into())
}

/// Rebuild a movie container with a new movie file spliced in: keep the fixed
/// `[UCFX header + single 'data' descriptor]`, patch the descriptor body_size,
/// append the new movie, append a freshly-computed CSUM trailer.
fn rebuild_container(orig: &[u8], new_movie: &[u8]) -> Result<Vec<u8>, String> {
    if orig.len() < 32 || &orig[0..4] != b"UCFX" {
        return Err("original container is not UCFX".into());
    }
    let data_area_off = read_u32_le(orig, 4) as usize;
    let n_desc = read_u32_le(orig, 16);
    if n_desc != 1 || &orig[20..24] != b"data" {
        return Err(format!(
            "not a simple movie container (n_desc={n_desc}, tag={:?}); rewrap unsupported",
            String::from_utf8_lossy(&orig[20..24])
        ));
    }
    let orig_body = read_u32_le(orig, 28) as usize;
    if data_area_off + orig_body + 8 != orig.len() {
        return Err(format!(
            "unexpected layout: header {data_area_off} + data {orig_body} + CSUM 8 != {}",
            orig.len()
        ));
    }
    let mut body = Vec::with_capacity(data_area_off + new_movie.len() + 8);
    body.extend_from_slice(&orig[..data_area_off]); // header + descriptor
    body[28..32].copy_from_slice(&(new_movie.len() as u32).to_le_bytes()); // patch data body_size
    body.extend_from_slice(new_movie);
    let crc = crc32_mercs2(&body);
    body.extend_from_slice(b"CSUM");
    body.extend_from_slice(&crc.to_le_bytes());
    Ok(body)
}

fn extract(wad: &PathBuf, name: &str, out: &PathBuf) -> Result<(), String> {
    let mut file = File::open(wad).map_err(|e| format!("open {}: {e}", wad.display()))?;
    let file_size = file.metadata().map_err(|e| format!("metadata: {e}"))?.len();
    let archive = load_ffcs_archive(&mut file, file_size).map_err(|e| format!("FFCS: {e}"))?;
    let loc = locate_movie(&mut file, &archive, name)?;
    let movie = container_movie(&loc.container)?;
    let hdr = find_movie(&movie)
        .map(|(o, m, v, l)| format!("{m} v{v} filelen={l} @0x{o:X}"))
        .unwrap_or_else(|| "?? (no recognizable movie header)".into());
    std::fs::write(out, &movie).map_err(|e| format!("write {}: {e}", out.display()))?;
    println!(
        "extracted '{name}' (0x{:08X}, type_id={}) from block[{}] -> {} ({} bytes; {hdr})",
        loc.name_hash,
        loc.type_id,
        loc.block_index,
        out.display(),
        movie.len()
    );
    Ok(())
}

fn build(wad: &PathBuf, name: &str, movie: &PathBuf, out: &PathBuf, merge: Option<&PathBuf>) -> Result<(), String> {
    let mut file = File::open(wad).map_err(|e| format!("open {}: {e}", wad.display()))?;
    let file_size = file.metadata().map_err(|e| format!("metadata: {e}"))?.len();
    let archive = load_ffcs_archive(&mut file, file_size).map_err(|e| format!("FFCS: {e}"))?;
    let loc = locate_movie(&mut file, &archive, name)?;
    let new_movie = std::fs::read(movie).map_err(|e| format!("read {}: {e}", movie.display()))?;
    if find_movie(&new_movie).is_none() {
        eprintln!("warning: {} has no GFX/CFX/FWS/CWS header — is it a real Scaleform/SWF file?", movie.display());
    }

    let container = rebuild_container(&loc.container, &new_movie)?;
    // block = [u32 count=1][name_hash][type_hash][field_c][chunk_size][container]
    let mut block = Vec::with_capacity(4 + 16 + container.len());
    block.extend_from_slice(&1u32.to_le_bytes());
    block.extend_from_slice(&loc.name_hash.to_le_bytes());
    block.extend_from_slice(&loc.type_hash.to_le_bytes());
    block.extend_from_slice(&loc.field_c.to_le_bytes());
    block.extend_from_slice(&(container.len() as u32).to_le_bytes());
    block.extend_from_slice(&container);

    let compressed = compress_sges(&block).map_err(|e| format!("sges: {e}"))?;
    let decomp_pages = ((block.len() + PAGE - 1) / PAGE) as u32;
    let aset = vec![AsetEntry::new(loc.name_hash, loc.secondary_ref, 0x0000_FFFF, loc.type_id)];
    let mut pb = PatchBlock::new(compressed, loc.block_path.clone(), aset);
    pb.packed_field = decomp_pages;

    let csum_value = find_chunk(&archive.chunks, b"CSUM").map(|r| r.offset).unwrap_or(0);
    let csum_meta = find_chunk(&archive.chunks, b"CSUM").map(|r| r.meta);

    let wad_bytes = if let Some(existing) = merge {
        let ex = std::fs::read(existing).map_err(|e| format!("read merge {}: {e}", existing.display()))?;
        merge_patch_wads(&ex, vec![pb], true)?
    } else {
        build_patch_wad_multi(&[pb], csum_value, csum_meta, &FFCS_CERT_BLOB)
    };
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    std::fs::write(out, &wad_bytes).map_err(|e| format!("write {}: {e}", out.display()))?;
    println!(
        "built patch: override '{name}' (0x{:08X}, type_id={}) with {} movie bytes -> {} ({} bytes)",
        loc.name_hash,
        loc.type_id,
        new_movie.len(),
        out.display(),
        wad_bytes.len()
    );
    Ok(())
}

fn find_assets(wad: &PathBuf, names: &[String]) -> Result<(), String> {
    let mut file = File::open(wad).map_err(|e| format!("open {}: {e}", wad.display()))?;
    let file_size = file.metadata().map_err(|e| format!("metadata: {e}"))?.len();
    let archive = load_ffcs_archive(&mut file, file_size).map_err(|e| format!("FFCS: {e}"))?;
    for name in names {
        let h = pandemic_hash_m2(name);
        let hits: Vec<_> = archive.aset.iter().filter(|e| e.asset_hash == h).collect();
        if hits.is_empty() {
            println!("{name:<28} -> 0x{h:08X}  (not in this WAD)");
        } else {
            for e in hits {
                let bidx = e.block_index() as usize;
                let path = archive.paths.get(bidx).cloned().unwrap_or_default();
                println!(
                    "{name:<28} -> 0x{h:08X}  block[{bidx}] type_id={} secondary=0x{:08X}  {path}",
                    e.type_id, e.secondary_ref
                );
            }
        }
    }
    Ok(())
}

fn resolve_block(paths: &[String], idx: Option<usize>, name: Option<&str>) -> Result<usize, String> {
    if let Some(i) = idx {
        return Ok(i);
    }
    if let Some(n) = name {
        let nl = n.to_lowercase();
        for (i, p) in paths.iter().enumerate() {
            if p.to_lowercase().contains(&nl) {
                return Ok(i);
            }
        }
        return Err(format!("no block path contains '{n}'"));
    }
    Err("need --block-index or --block-name".into())
}

fn inspect(wad: &PathBuf, idx: Option<usize>, name: Option<&str>) -> Result<(), String> {
    let mut file = File::open(wad).map_err(|e| format!("open {}: {e}", wad.display()))?;
    let file_size = file.metadata().map_err(|e| format!("metadata: {e}"))?.len();
    let archive = load_ffcs_archive(&mut file, file_size).map_err(|e| format!("FFCS: {e}"))?;

    let bidx = resolve_block(&archive.paths, idx, name)?;
    let path = archive
        .paths
        .get(bidx)
        .cloned()
        .unwrap_or_else(|| format!("block_{bidx}"));
    println!("block [{bidx}] {path}");

    let block = decompress_block(&mut file, &archive.indx, bidx as u16)
        .map_err(|e| format!("decompress block {bidx}: {e}"))?;
    println!("decompressed block: {} bytes", block.len());

    let (parsed, issues) = walk_decompressed_block(&block, &path);
    println!("entry_count: {}", parsed.entry_count);
    for (i, e) in parsed.entries.iter().enumerate() {
        println!(
            "  entry[{i}]: name=0x{:08X} type=0x{:08X} type_id={:?} field_c=0x{:08X} chunk_size={}",
            e.name_hash,
            e.type_hash,
            type_id_for_type_hash(e.type_hash),
            e.field_c,
            e.chunk_size
        );
    }
    if !issues.is_empty() {
        println!("walk issues: {}", issues.len());
        for is in issues.iter().take(8) {
            println!("   ! {}: {}", is.context, is.detail);
        }
    }

    for (i, c) in parsed.containers.iter().enumerate() {
        println!("--- container[{i}] len={} ---", c.len());
        println!("  head16: {}", hex(c, 16));
        let is_ucfx = c.len() >= 4 && &c[0..4] == b"UCFX";
        println!("  is UCFX: {is_ucfx}");
        if is_ucfx && c.len() >= 20 {
            let data_area_off = read_u32_le(c, 4);
            let n_desc = read_u32_le(c, 16);
            println!("  data_area_off=0x{data_area_off:X}  n_desc={n_desc}");
            let max_desc = c.len().saturating_sub(20) / 20;
            for d in 0..(n_desc as usize).min(max_desc) {
                let row = 20 + d * 20;
                let tag = String::from_utf8_lossy(&c[row..row + 4]).into_owned();
                let row_u0 = read_u32_le(c, row + 4);
                let body_size = read_u32_le(c, row + 8);
                let bstart = if data_area_off > 0 {
                    data_area_off.wrapping_add(row_u0)
                } else {
                    8u32.wrapping_add(row_u0)
                };
                let peek = if row_u0 != 0xFFFF_FFFF && (bstart as usize) < c.len() {
                    let end = (bstart as usize + 8).min(c.len());
                    find_movie(&c[bstart as usize..end]).map(|(_, m, v, _)| format!("{m} v{v}"))
                } else {
                    None
                };
                println!(
                    "    desc[{d}] tag='{tag}' row_u0=0x{row_u0:X} body_size={body_size} body_start=0x{bstart:X} movieHere={peek:?}"
                );
            }
        }
        match find_movie(c) {
            Some((off, m, ver, len)) => {
                println!("  MOVIE '{m}' v{ver} filelen={len} at container offset 0x{off:X}")
            }
            None => println!("  no movie header in container"),
        }
        if let Some(dc) = extract_data_chunk(c) {
            let m = find_movie(&dc).map(|(o, mm, v, _)| format!("{mm} v{v} @0x{o:X}"));
            println!("  data-chunk: {} bytes  movie={:?}  head16={}", dc.len(), m, hex(&dc, 16));
        } else {
            println!("  (no `data` descriptor chunk)");
        }
        if c.len() >= 8 && &c[c.len() - 8..c.len() - 4] == b"CSUM" {
            let stored = read_u32_le(c, c.len() - 4);
            let calc = crc32_mercs2(&c[..c.len() - 8]);
            println!("  CSUM trailer: stored=0x{stored:08X} calc=0x{calc:08X} match={}", stored == calc);
        } else {
            println!("  no CSUM trailer");
        }
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let r = match &cli.cmd {
        Cmd::Inspect { wad, block_index, block_name } => {
            inspect(wad, *block_index, block_name.as_deref())
        }
        Cmd::Find { wad, names } => find_assets(wad, names),
        Cmd::Extract { wad, name, out } => extract(wad, name, out),
        Cmd::Build { wad, name, movie, out, merge } => build(wad, name, movie, out, merge.as_ref()),
    };
    match r {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
