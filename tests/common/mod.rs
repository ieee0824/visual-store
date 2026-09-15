#![allow(dead_code)]
use flate2::{Compression, write::ZlibEncoder};
use serde_json::Value;
use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;
use visual_store::image::write_chunk;

pub fn samples(width: u32, height: u32, rgba: bool, noise: bool) -> Vec<u8> {
    let channels = if rgba { 4 } else { 3 };
    let mut seed = 1234567u32;
    (0..width as usize * height as usize * channels)
        .map(|i| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            if rgba && i % 4 == 3 {
                0
            } else if noise {
                seed as u8
            } else {
                (i % channels * 31 + 61) as u8
            }
        })
        .collect()
}
pub fn fixture(width: u32, height: u32, rgba: bool, noise: bool) -> Vec<u8> {
    let pix = samples(width, height, rgba, noise);
    let stride = width as usize * if rgba { 4 } else { 3 };
    let mut scan = Vec::new();
    for row in pix.chunks(stride) {
        scan.push(0);
        scan.extend_from_slice(row);
    }
    from_scan(width, height, if rgba { 6 } else { 2 }, 8, 0, &scan)
}
pub fn from_scan(
    width: u32,
    height: u32,
    color: u8,
    depth: u8,
    interlace: u8,
    scan: &[u8],
) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut h = Vec::new();
    h.extend_from_slice(&width.to_be_bytes());
    h.extend_from_slice(&height.to_be_bytes());
    h.extend_from_slice(&[depth, color, 0, 0, interlace]);
    write_chunk(&mut bytes, b"IHDR", &h);
    let mut z = ZlibEncoder::new(Vec::new(), Compression::none());
    z.write_all(scan).unwrap();
    write_chunk(&mut bytes, b"IDAT", &z.finish().unwrap());
    write_chunk(&mut bytes, b"IEND", &[]);
    bytes
}
pub fn decode(bytes: &[u8]) -> Vec<u8> {
    let mut r = png::Decoder::new(Cursor::new(bytes)).read_info().unwrap();
    let mut pixels = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut pixels).unwrap();
    pixels.truncate(info.buffer_size());
    pixels
}
pub fn chunks(bytes: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let mut out = Vec::new();
    let mut p = 8;
    while p < bytes.len() {
        let n = u32::from_be_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
        out.push((
            bytes[p + 4..p + 8].try_into().unwrap(),
            bytes[p + 8..p + 8 + n].to_vec(),
        ));
        p += n + 12;
    }
    out
}
pub fn rebuild(parts: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
    for (k, d) in parts {
        write_chunk(&mut b, k, d);
    }
    b
}
pub fn add_chunk(bytes: &[u8], kind: [u8; 4], data: Vec<u8>) -> Vec<u8> {
    let mut c = chunks(bytes);
    c.insert(1, (kind, data));
    rebuild(&c)
}

pub struct Harness {
    pub temp: TempDir,
    pub root: PathBuf,
    pub input: PathBuf,
}
impl Harness {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("store");
        let input = temp.path().join("input.png");
        fs::write(&input, fixture(32, 24, true, false)).unwrap();
        Self { temp, root, input }
    }
    pub fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vstore"));
        c.arg("--store").arg(&self.root);
        c.env_remove("VSTORE_ROOT");
        c
    }
    pub fn raw(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    pub fn call(&self, args: &[&str]) -> Value {
        let o = self.raw(args);
        assert!(
            o.status.success(),
            "args {args:?}: {}",
            String::from_utf8_lossy(&o.stdout)
        );
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        assert_eq!(v["ok"], true);
        v["data"].clone()
    }
    pub fn error(&self, args: &[&str], code: &str) -> Value {
        let o = self.raw(args);
        assert!(
            !o.status.success(),
            "unexpected success: {}",
            String::from_utf8_lossy(&o.stdout)
        );
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        assert_eq!(v["error"]["code"], code, "{v}");
        v
    }
    pub fn init(&self) {
        self.call(&["init"]);
    }
    pub fn put(&self) -> Value {
        self.call(&["put", "--file", self.input.to_str().unwrap()])
    }
    pub fn blob(&self, hash: &str) -> PathBuf {
        self.root.join(format!(
            "objects/sha256/{}/{}/{hash}.png",
            &hash[..2],
            &hash[2..4]
        ))
    }
}
pub fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dst.join(e.file_name()));
        } else {
            fs::copy(e.path(), dst.join(e.file_name())).unwrap();
        }
    }
}
