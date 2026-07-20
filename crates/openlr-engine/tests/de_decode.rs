//! Integration test: decode a real DE reference against the actual tile data.
//! Run with: cargo test -p openlr-engine de_decode -- --nocapture

use openlr_codec::decode_v3_base64;
use openlr_engine::{decode, DecodeParams, Preset, prefetch_tile_keys};
use openlr_provider::TileLoader;

const DE_TILES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../out/de-osm/openlrlab-de-germany-latest.pmtiles"
);

fn read_tile(pmtiles_path: &str, z: u8, x: u32, y: u32) -> Option<Vec<u8>> {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};

    let mut f = File::open(pmtiles_path).ok()?;
    let mut hdr = [0u8; 127];
    f.read_exact(&mut hdr).ok()?;

    let root_offset = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
    let root_len    = u64::from_le_bytes(hdr[16..24].try_into().unwrap());
    let leaf_base   = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
    let data_base   = u64::from_le_bytes(hdr[56..64].try_into().unwrap());

    f.seek(SeekFrom::Start(root_offset)).ok()?;
    let mut root_c = vec![0u8; root_len as usize];
    f.read_exact(&mut root_c).ok()?;
    let root_raw = {
        use std::io::Read as _;
        let mut dec = flate2::read::GzDecoder::new(root_c.as_slice());
        let mut out = Vec::new();
        dec.read_to_end(&mut out).ok()?;
        out
    };

    fn read_varint(buf: &[u8], mut pos: usize) -> (u64, usize) {
        let mut r = 0u64; let mut shift = 0;
        loop {
            let b = buf[pos]; pos += 1;
            r |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 { break; }
            shift += 7;
        }
        (r, pos)
    }

    fn decode_dir(raw: &[u8]) -> Vec<(u64, u32, u32, u64)> { // (id, rl, len, offset)
        let mut pos = 0;
        let (n, p) = read_varint(raw, pos); pos = p;
        let mut ids = Vec::with_capacity(n as usize);
        let mut last = 0u64;
        for _ in 0..n {
            let (d, p) = read_varint(raw, pos); pos = p; last += d; ids.push(last);
        }
        let mut rls = Vec::with_capacity(n as usize);
        for _ in 0..n { let (v, p) = read_varint(raw, pos); pos = p; rls.push(v as u32); }
        let mut lens = Vec::with_capacity(n as usize);
        for _ in 0..n { let (v, p) = read_varint(raw, pos); pos = p; lens.push(v as u32); }
        let mut offsets = Vec::with_capacity(n as usize);
        let mut prev_off = 0u64; let mut prev_len = 0u64;
        for i in 0..n as usize {
            let (v, p) = read_varint(raw, pos); pos = p;
            let off = if v == 0 && i > 0 { prev_off + prev_len } else { v - 1 };
            offsets.push(off);
            prev_off = off; prev_len = lens[i] as u64;
        }
        ids.into_iter().zip(rls).zip(lens).zip(offsets)
            .map(|(((id, rl), len), off)| (id, rl, len, off))
            .collect()
    }

    fn hilbert_d(n: u64, mut x: u64, mut y: u64) -> u64 {
        let mut s = n >> 1; let mut d = 0u64;
        while s > 0 {
            let rx = if x & s > 0 { 1u64 } else { 0 };
            let ry = if y & s > 0 { 1u64 } else { 0 };
            d += s * s * ((3 * rx) ^ ry);
            if ry == 0 { if rx == 1 { x = n-1-x; y = n-1-y; } std::mem::swap(&mut x, &mut y); }
            s >>= 1;
        }
        d
    }
    let acc = ((1u64 << (2 * z as u32)) - 1) / 3;
    let target_id = acc + hilbert_d(1 << z, x as u64, y as u64);

    let root_entries = decode_dir(&root_raw);
    let leaf_ptr = root_entries.iter().find(|e| e.1 == 0 && e.0 <= target_id);

    let entries = if let Some(lp) = leaf_ptr {
        f.seek(SeekFrom::Start(leaf_base + lp.3)).ok()?;
        let mut lc = vec![0u8; lp.2 as usize];
        f.read_exact(&mut lc).ok()?;
        let lr = {
            use std::io::Read as _;
            let mut dec = flate2::read::GzDecoder::new(lc.as_slice());
            let mut out = Vec::new();
            dec.read_to_end(&mut out).ok()?;
            out
        };
        decode_dir(&lr)
    } else {
        root_entries
    };

    let entry = entries.iter().find(|e| e.1 >= 1 && e.0 == target_id)?;
    f.seek(SeekFrom::Start(data_base + entry.3)).ok()?;
    let mut tile = vec![0u8; entry.2 as usize];
    f.read_exact(&mut tile).ok()?;
    Some(tile)
}

#[test]
fn decode_de_reference() {
    let tile_path = DE_TILES;
    if !std::path::Path::new(tile_path).exists() {
        println!("SKIP: DE tiles not found at {tile_path}");
        return;
    }

    let loc_ref = decode_v3_base64("CwV1BCHeEDv1BQEj/3s7WiY=")
        .expect("v3 parse failed");
    let lrps = loc_ref.lrps().expect("expected network location type");
    println!("LRP0 coord: {:?}", lrps[0].coord);
    println!("LRP0 bearing: {:?}", lrps[0].bearing);
    println!("LRP0 frc={} fow={}", lrps[0].frc, lrps[0].fow);

    let params = DecodeParams::preset(Preset::Permissive);
    let tile_keys = prefetch_tile_keys(lrps, &params, 12);
    println!("Fetching {} tiles", tile_keys.len());

    let mut loader = TileLoader::new();
    let mut loaded = 0usize;
    for key in &tile_keys {
        if let Some(data) = read_tile(tile_path, key.z, key.x, key.y) {
            println!("  tile {}/{}/{}: {} bytes, magic={:?}", key.z, key.x, key.y, data.len(), &data[..4]);
            match loader.load_tile(&data) {
                Ok(_)   => { loaded += 1; }
                Err(e)  => println!("  LOAD ERROR: {e}"),
            }
        } else {
            println!("  tile {}/{}/{}: NOT IN ARCHIVE", key.z, key.x, key.y);
        }
    }
    println!("Loaded {loaded}/{} tiles → {} segments, {} nodes",
        tile_keys.len(), loader.graph.segments.len(), loader.graph.nodes.len());

    let result = decode(&loc_ref, &loader.graph, &params, 12);
    match result {
        Ok(r)  => {
            let n_segs = r.as_network().map(|d| d.path.len()).unwrap_or(0);
            println!("DECODE OK: {n_segs} segments in path");
        }
        Err(e) => println!("DECODE ERROR: {e}"),
    }
}
