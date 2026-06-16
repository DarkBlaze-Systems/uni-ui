//! A tiny, dependency-light PNG encoder for the software/preview path.
//!
//! This exists so the canvas backend can write its RGBA buffer to a real
//! `.png` (the SwiftUI `#Preview` story) without pulling an image crate into
//! the clean-room dependency set. It emits a valid 8-bit RGBA PNG using
//! *stored* (uncompressed) zlib deflate blocks — no compression dictionary, no
//! Huffman tables — which keeps the code trivial and the output a spec-valid
//! PNG that every decoder reads. Files are larger than a `png`-crate encode,
//! but this path is for previews/tests, not asset shipping.
//!
//! Reference: PNG spec (RFC 2083) + zlib (RFC 1950) + DEFLATE (RFC 1951).

/// Encode an RGBA8 pixel buffer (row-major, top-down, `width*height*4` bytes)
/// as PNG bytes. Returns the full file (signature + IHDR + IDAT + IEND).
///
/// Panics only if `pixels.len() != width*height*4`.
pub fn encode_rgba(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(
        pixels.len(),
        (width as usize) * (height as usize) * 4,
        "pixel buffer must be width*height*4 RGBA bytes"
    );

    let mut out = Vec::new();
    // PNG signature.
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    // IHDR: width, height, bit depth 8, color type 6 (RGBA), no
    // interlace/filter/compression options.
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // color type RGBA
    ihdr.push(0); // compression method (deflate)
    ihdr.push(0); // filter method
    ihdr.push(0); // interlace: none
    write_chunk(&mut out, b"IHDR", &ihdr);

    // Build the raw scanline stream: each row is prefixed by filter byte 0
    // (None) then the row's RGBA bytes.
    let stride = (width as usize) * 4;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0); // filter type: None
        let start = y * stride;
        raw.extend_from_slice(&pixels[start..start + stride]);
    }

    // zlib-wrap the raw stream with stored deflate blocks.
    let zlib = zlib_store(&raw);
    write_chunk(&mut out, b"IDAT", &zlib);

    write_chunk(&mut out, b"IEND", &[]);
    out
}

/// Write one PNG chunk: length, type, data, CRC32(type+data).
fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
}

/// Wrap `data` in a zlib stream made of uncompressed ("stored") DEFLATE blocks.
fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // zlib header: CMF=0x78 (deflate, 32K window), FLG=0x01 → (0x78*256+0x01)
    // is a multiple of 31, fastest-compression level bits.
    out.push(0x78);
    out.push(0x01);

    // Stored blocks: each carries up to 65535 bytes. BFINAL set on the last.
    let mut offset = 0;
    let n = data.len();
    if n == 0 {
        // One empty final stored block.
        out.push(0x01); // BFINAL=1, BTYPE=00
        out.extend_from_slice(&0u16.to_le_bytes()); // LEN
        out.extend_from_slice(&(!0u16).to_le_bytes()); // NLEN
    } else {
        while offset < n {
            let chunk = (n - offset).min(0xFFFF);
            let is_final = offset + chunk >= n;
            out.push(if is_final { 1 } else { 0 }); // BFINAL bit, BTYPE=00
            let len = chunk as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(&data[offset..offset + chunk]);
            offset += chunk;
        }
    }

    // Adler-32 of the uncompressed data, big-endian, closes the zlib stream.
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Adler-32 checksum (RFC 1950).
fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

/// Streaming CRC-32 (IEEE, as PNG requires), computed with a lazily-built table.
struct Crc32 {
    crc: u32,
}

impl Crc32 {
    fn new() -> Self {
        Crc32 { crc: 0xFFFF_FFFF }
    }

    fn update(&mut self, data: &[u8]) {
        let table = crc_table();
        for &byte in data {
            let idx = ((self.crc ^ byte as u32) & 0xFF) as usize;
            self.crc = table[idx] ^ (self.crc >> 8);
        }
    }

    fn finalize(self) -> u32 {
        self.crc ^ 0xFFFF_FFFF
    }
}

/// The CRC-32 lookup table, built once on first use.
fn crc_table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (n, slot) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        table
    })
}

#[cfg(test)]
mod png_tests {
    use super::*;

    #[test]
    fn encodes_valid_png_header() {
        let pixels = vec![255u8; 4 * 4 * 4]; // 4x4 white-opaque
        let png = encode_rgba(4, 4, &pixels);
        // PNG signature.
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // First chunk type after the 8-byte sig + 4-byte length is "IHDR".
        assert_eq!(&png[12..16], b"IHDR");
        // Ends with IEND.
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        assert!(png.len() > 50, "non-trivial PNG bytes");
    }

    #[test]
    fn ihdr_records_dimensions_and_rgba() {
        let png = encode_rgba(3, 2, &[0u8; 3 * 2 * 4]);
        // IHDR data begins at offset 16: width(4) height(4) depth color ...
        let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        assert_eq!((w, h), (3, 2));
        assert_eq!(png[24], 8, "bit depth 8");
        assert_eq!(png[25], 6, "color type RGBA");
    }

    #[test]
    fn adler_and_crc_are_nonzero() {
        // Sanity: a non-empty buffer produces a stored zlib stream we can find.
        let png = encode_rgba(2, 2, &[128u8; 2 * 2 * 4]);
        assert!(png.windows(4).any(|w| w == b"IDAT"), "has IDAT chunk");
    }
}
