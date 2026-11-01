//! Small file writers the shelf needs and Windows lacks an encoder for:
//! lossless WebP (VP8L), ICO (PNG entries) and PDF (one image per page, or
//! plain text in Courier) for "Convert to", and zip for the Zip Quick Action.

// ---------------------------------------------------------------------------
// WebP (lossless VP8L): subtract-green and an Average(L, T) predictor, then
// per-channel Huffman codes. No backward references, so it's quick to write
// and still compresses flat artwork and screenshots well.
// ---------------------------------------------------------------------------

/// Largest side VP8L can describe.
pub const WEBP_MAX: u32 = 16384;

/// Encodes BGRA pixels (`w * h * 4` bytes) as a lossless WebP file.
pub fn webp(bgra: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || w > WEBP_MAX || h > WEBP_MAX || bgra.len() < (w * h * 4) as usize {
        return None;
    }
    let argb: Vec<u32> = bgra.chunks_exact(4).take((w * h) as usize).map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]])).collect();
    let alpha = argb.iter().any(|p| p >> 24 != 0xff);
    // Subtract green: red and blue store their difference from green.
    let sg: Vec<u32> = argb
        .iter()
        .map(|&p| {
            let g = (p >> 8) & 0xff;
            let r = ((p >> 16) & 0xff).wrapping_sub(g) & 0xff;
            let b = (p & 0xff).wrapping_sub(g) & 0xff;
            (p & 0xff00ff00) | (r << 16) | b
        })
        .collect();
    // Predictor: every pixel stores its difference from Average2(left, top).
    let (wu, hu) = (w as usize, h as usize);
    let mut res = vec![0u32; sg.len()];
    for y in 0..hu {
        for x in 0..wu {
            let i = y * wu + x;
            let pred = match (x, y) {
                (0, 0) => 0xff000000,
                (_, 0) => sg[i - 1],
                (0, _) => sg[i - wu],
                _ => average2(sg[i - 1], sg[i - wu]),
            };
            res[i] = sub_pixels(sg[i], pred);
        }
    }

    let mut bw = BitWriter::default();
    bw.put(0x2f, 8);
    bw.put(w - 1, 14);
    bw.put(h - 1, 14);
    bw.put(alpha as u32, 1);
    bw.put(0, 3);
    // Transform 1: subtract green.
    bw.put(1, 1);
    bw.put(2, 2);
    // Transform 2: predictor with the largest blocks, all mode 7.
    const BITS: u32 = 9;
    bw.put(1, 1);
    bw.put(0, 2);
    bw.put(BITS - 2, 3);
    let (bw_n, bh_n) = (w.div_ceil(1 << BITS), h.div_ceil(1 << BITS));
    let modes = vec![0xff000000 | (7 << 8); (bw_n * bh_n) as usize];
    entropy_image(&mut bw, &modes, false);
    bw.put(0, 1);
    entropy_image(&mut bw, &res, true);
    let data = bw.finish();

    let mut chunk = data.len() as u32;
    let pad = chunk & 1;
    chunk += pad;
    let mut out = Vec::with_capacity(20 + chunk as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(4 + 8 + chunk).to_le_bytes());
    out.extend_from_slice(b"WEBPVP8L");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    if pad == 1 {
        out.push(0);
    }
    Some(out)
}

fn average2(a: u32, b: u32) -> u32 {
    (((a ^ b) & 0xfefefefe) >> 1) + (a & b)
}

fn sub_pixels(a: u32, b: u32) -> u32 {
    let mut out = 0;
    for s in [0, 8, 16, 24] {
        out |= (((a >> s) & 0xff).wrapping_sub((b >> s) & 0xff) & 0xff) << s;
    }
    out
}

/// One entropy-coded image: no color cache, (main image only) no meta codes,
/// five prefix codes, then every pixel as four literals.
fn entropy_image(bw: &mut BitWriter, px: &[u32], main: bool) {
    bw.put(0, 1);
    if main {
        bw.put(0, 1);
    }
    let channel = |p: u32, i: usize| match i {
        0 => (p >> 8) & 0xff,
        1 => (p >> 16) & 0xff,
        2 => p & 0xff,
        _ => p >> 24,
    } as usize;
    let mut codes = Vec::new();
    for i in 0..4 {
        let mut counts = vec![0u32; if i == 0 { 280 } else { 256 }];
        for &p in px {
            counts[channel(p, i)] += 1;
        }
        codes.push(write_code(bw, &counts));
    }
    // Distance codes are never used.
    write_code(bw, &[1]);
    for &p in px {
        for (i, code) in codes.iter().enumerate() {
            code.put(bw, channel(p, i));
        }
    }
}

/// Canonical prefix code; a single used symbol takes no bits.
struct Code {
    codes: Vec<(u32, u32)>,
}

impl Code {
    fn from_lengths(lengths: &[u8]) -> Code {
        let used = lengths.iter().filter(|&&l| l > 0).count();
        let mut codes = vec![(0, 0); lengths.len()];
        if used <= 1 {
            return Code { codes };
        }
        let max = *lengths.iter().max().unwrap_or(&0) as usize;
        let mut count = vec![0u32; max + 1];
        for &l in lengths {
            if l > 0 {
                count[l as usize] += 1;
            }
        }
        let mut next = vec![0u32; max + 2];
        let mut code = 0;
        for bits in 1..=max {
            code = (code + count[bits - 1]) << 1;
            next[bits] = code;
        }
        for (s, &l) in lengths.iter().enumerate() {
            if l > 0 {
                let c = next[l as usize];
                next[l as usize] += 1;
                // Stored most significant bit first, so reverse for the LSB-first writer.
                codes[s] = (c.reverse_bits() >> (32 - l as u32), l as u32);
            }
        }
        Code { codes }
    }

    fn put(&self, bw: &mut BitWriter, sym: usize) {
        let (c, n) = self.codes[sym];
        bw.put(c, n);
    }
}

/// Writes the prefix code for `counts` and returns it.
fn write_code(bw: &mut BitWriter, counts: &[u32]) -> Code {
    let used: Vec<usize> = (0..counts.len()).filter(|&s| counts[s] > 0).collect();
    if used.len() <= 2 && used.iter().all(|&s| s < 256) {
        // Simple code: one or two literal symbols.
        bw.put(1, 1);
        match used.as_slice() {
            [] => bw.put(0, 3),
            [a, ..] => {
                bw.put(used.len() as u32 - 1, 1);
                let wide = *a > 1;
                bw.put(wide as u32, 1);
                bw.put(*a as u32, if wide { 8 } else { 1 });
                if let [_, b] = used.as_slice() {
                    bw.put(*b as u32, 8);
                }
            }
        }
        let mut lengths = vec![0u8; counts.len()];
        if let [a, b] = used.as_slice() {
            lengths[*a] = 1;
            lengths[*b] = 1;
        }
        return Code::from_lengths(&lengths);
    }
    let lengths = huffman_lengths(counts, 15);
    // Normal code: the code lengths, themselves Huffman coded (symbols 0-15 only).
    const ORDER: [usize; 19] = [17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut cl_counts = [0u32; 19];
    for &l in &lengths {
        cl_counts[l as usize] += 1;
    }
    let mut cl_lengths = huffman_lengths(&cl_counts, 7);
    if cl_lengths.iter().filter(|&&l| l > 0).count() == 1 {
        // One length used everywhere: it's read with zero bits per symbol.
        if let Some(l) = cl_lengths.iter_mut().find(|l| **l > 0) {
            *l = 1;
        }
    }
    let n = ORDER.iter().rposition(|&s| cl_lengths[s] > 0).map_or(4, |i| (i + 1).max(4));
    bw.put(0, 1);
    bw.put(n as u32 - 4, 4);
    for &s in &ORDER[..n] {
        bw.put(cl_lengths[s] as u32, 3);
    }
    bw.put(0, 1);
    let cl = Code::from_lengths(&cl_lengths);
    for &l in &lengths {
        cl.put(bw, l as usize);
    }
    Code::from_lengths(&lengths)
}

/// Huffman code lengths no longer than `limit` (flattening the counts until they fit).
fn huffman_lengths(counts: &[u32], limit: u8) -> Vec<u8> {
    let mut counts: Vec<u64> = counts.iter().map(|&c| c as u64).collect();
    loop {
        let lengths = huffman_once(&counts);
        if lengths.iter().all(|&l| l <= limit) {
            return lengths;
        }
        for c in counts.iter_mut().filter(|c| **c > 0) {
            *c = (*c >> 1).max(1);
        }
    }
}

fn huffman_once(counts: &[u64]) -> Vec<u8> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let mut lengths = vec![0u8; counts.len()];
    let used: Vec<usize> = (0..counts.len()).filter(|&s| counts[s] > 0).collect();
    if used.len() == 1 {
        lengths[used[0]] = 1;
        return lengths;
    }
    // Nodes: leaves first, then merged ones; `parent` links walk back up.
    let mut parent: Vec<usize> = vec![usize::MAX; used.len()];
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = used.iter().enumerate().map(|(i, &s)| Reverse((counts[s], i))).collect();
    while heap.len() > 1 {
        let Reverse((a, ia)) = heap.pop().unwrap();
        let Reverse((b, ib)) = heap.pop().unwrap();
        let id = parent.len();
        parent.push(usize::MAX);
        parent[ia] = id;
        parent[ib] = id;
        heap.push(Reverse((a + b, id)));
    }
    for (i, &s) in used.iter().enumerate() {
        let (mut n, mut depth) = (i, 0u8);
        while parent[n] != usize::MAX {
            n = parent[n];
            depth = depth.saturating_add(1);
        }
        lengths[s] = depth;
    }
    lengths
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        self.acc |= ((v as u64) & ((1u64 << bits) - 1)) << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

// ---------------------------------------------------------------------------
// ICO: PNG-compressed entries (Windows Vista and later read these).
// ---------------------------------------------------------------------------

/// An .ico holding `(side, png bytes)` images.
pub fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (side, png) in images {
        let s = if *side >= 256 { 0 } else { *side as u8 };
        out.extend_from_slice(&[s, s, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in images {
        out.extend_from_slice(png);
    }
    out
}

// ---------------------------------------------------------------------------
// PDF
// ---------------------------------------------------------------------------

struct Pdf {
    out: Vec<u8>,
    offsets: Vec<usize>,
}

impl Pdf {
    fn new() -> Pdf {
        Pdf { out: b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec(), offsets: Vec::new() }
    }

    /// Objects are numbered in the order they're added, from 1.
    fn obj(&mut self, body: &[u8]) {
        self.offsets.push(self.out.len());
        self.out.extend_from_slice(format!("{} 0 obj\n", self.offsets.len()).as_bytes());
        self.out.extend_from_slice(body);
        self.out.extend_from_slice(b"\nendobj\n");
    }

    fn stream(&mut self, dict: &str, data: &[u8]) {
        let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        self.obj(&body);
    }

    fn finish(mut self) -> Vec<u8> {
        let xref = self.out.len();
        let n = self.offsets.len() + 1;
        self.out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
        for o in &self.offsets {
            self.out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        self.out.extend_from_slice(format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        self.out
    }
}

/// A one-page PDF showing a JPEG at `w_pt` x `h_pt` points.
pub fn pdf_jpeg(jpeg: &[u8], px_w: u32, px_h: u32, w_pt: f32, h_pt: f32) -> Vec<u8> {
    let mut pdf = Pdf::new();
    pdf.obj(b"<< /Type /Catalog /Pages 2 0 R >>");
    pdf.obj(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    pdf.obj(
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w_pt:.2} {h_pt:.2}] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>")
            .as_bytes(),
    );
    pdf.stream(
        &format!("/Type /XObject /Subtype /Image /Width {px_w} /Height {px_h} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode"),
        jpeg,
    );
    pdf.stream("", format!("q {w_pt:.2} 0 0 {h_pt:.2} 0 0 cm /Im0 Do Q").as_bytes());
    pdf.finish()
}

/// A4 pages of `text` in 10 pt Courier, wrapped at the margin.
pub fn pdf_text(text: &str) -> Vec<u8> {
    const W: f32 = 595.0;
    const H: f32 = 842.0;
    const MARGIN: f32 = 56.0;
    const SIZE: f32 = 10.0;
    const LEAD: f32 = 12.0;
    let cols = ((W - 2.0 * MARGIN) / (SIZE * 0.6)) as usize;
    let rows = ((H - 2.0 * MARGIN) / LEAD) as usize;
    let mut lines = Vec::new();
    for line in text.replace("\r\n", "\n").replace('\t', "    ").split('\n') {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            lines.push(String::new());
        }
        for chunk in chars.chunks(cols) {
            lines.push(chunk.iter().collect());
        }
    }
    let pages: Vec<&[String]> = if lines.is_empty() { vec![&[][..]] } else { lines.chunks(rows).collect() };

    let mut pdf = Pdf::new();
    let n = pages.len();
    // 1 catalog, 2 pages, 3 font, then a page and its content per page.
    pdf.obj(b"<< /Type /Catalog /Pages 2 0 R >>");
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    pdf.obj(format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")).as_bytes());
    pdf.obj(b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier /Encoding /WinAnsiEncoding >>");
    for (i, page) in pages.iter().enumerate() {
        pdf.obj(
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {W} {H}] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>", 5 + 2 * i)
                .as_bytes(),
        );
        let mut s = format!("BT /F1 {SIZE} Tf {LEAD} TL {MARGIN} {} Td\n", H - MARGIN - SIZE).into_bytes();
        for line in page.iter() {
            s.push(b'(');
            s.extend(pdf_string(line));
            s.extend_from_slice(b") '\n");
        }
        s.extend_from_slice(b"ET");
        pdf.stream("", &s);
    }
    pdf.finish()
}

/// A string literal's bytes in WinAnsi (Latin-1 here), escaped; others become '?'.
fn pdf_string(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '(' | ')' | '\\' => {
                out.push(b'\\');
                out.push(c as u8);
            }
            ' '..='~' | '\u{A0}'..='\u{FF}' => out.push(c as u32 as u8),
            _ => out.push(b'?'),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Zip: deflated entries with UTF-8 names (general purpose bit 11) and sizes in
// a data descriptor after each entry, so files stream through without being
// read twice. No Zip64, so entries and the archive stay under 4 GB.
// ---------------------------------------------------------------------------

struct ZipEntry {
    name: String,
    dir: bool,
    time: (u16, u16),
    crc: u32,
    packed: u32,
    size: u32,
    offset: u32,
}

/// Zips `paths` (files and folders, each under its own name) into `out`.
pub fn zip(paths: &[std::path::PathBuf], out: &std::path::Path) -> anyhow::Result<()> {
    use std::io::Write;
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    let mut entries = Vec::new();
    let mut pos = 0u64;
    for p in paths {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        zip_add(&mut w, &mut pos, &mut entries, p, name)?;
    }
    let dir_at = zip_u32(pos)?;
    let mut central = Vec::new();
    for e in &entries {
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0]);
        central.extend_from_slice(&zip_flags(e.dir).to_le_bytes());
        central.extend_from_slice(&(if e.dir { 0u16 } else { 8 }).to_le_bytes());
        central.extend_from_slice(&e.time.0.to_le_bytes());
        central.extend_from_slice(&e.time.1.to_le_bytes());
        for v in [e.crc, e.packed, e.size] {
            central.extend_from_slice(&v.to_le_bytes());
        }
        central.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        // Extra and comment lengths, disk number, internal attributes.
        central.extend_from_slice(&[0; 8]);
        // External attributes: the DOS directory bit.
        central.extend_from_slice(&(if e.dir { 0x10u32 } else { 0 }).to_le_bytes());
        central.extend_from_slice(&e.offset.to_le_bytes());
        central.extend_from_slice(e.name.as_bytes());
    }
    let count = u16::try_from(entries.len()).map_err(|_| anyhow::anyhow!("too many files to zip (65,535 at most)"))?;
    let mut end = Vec::with_capacity(22);
    end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    end.extend_from_slice(&[0; 4]);
    end.extend_from_slice(&count.to_le_bytes());
    end.extend_from_slice(&count.to_le_bytes());
    end.extend_from_slice(&(central.len() as u32).to_le_bytes());
    end.extend_from_slice(&dir_at.to_le_bytes());
    end.extend_from_slice(&[0; 2]);
    w.write_all(&central)?;
    w.write_all(&end)?;
    w.flush()?;
    Ok(())
}

fn zip_flags(dir: bool) -> u16 {
    // Bit 11: the name is UTF-8. Bit 3: sizes follow the data.
    if dir { 0x0800 } else { 0x0808 }
}

fn zip_u32(n: u64) -> anyhow::Result<u32> {
    u32::try_from(n).map_err(|_| anyhow::anyhow!("zips over 4 GB aren't supported yet"))
}

fn zip_add(
    w: &mut impl std::io::Write,
    pos: &mut u64,
    entries: &mut Vec<ZipEntry>,
    path: &std::path::Path,
    name: String,
) -> anyhow::Result<()> {
    let meta = std::fs::metadata(path)?;
    let dir = meta.is_dir();
    let name = if dir { format!("{name}/") } else { name };
    let time = dos_time(meta.modified().unwrap_or_else(|_| std::time::SystemTime::now()));
    let offset = zip_u32(*pos)?;
    let mut local = Vec::with_capacity(30 + name.len());
    local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    local.extend_from_slice(&20u16.to_le_bytes());
    local.extend_from_slice(&zip_flags(dir).to_le_bytes());
    local.extend_from_slice(&(if dir { 0u16 } else { 8 }).to_le_bytes());
    local.extend_from_slice(&time.0.to_le_bytes());
    local.extend_from_slice(&time.1.to_le_bytes());
    // CRC and sizes: zero here, in the data descriptor instead.
    local.extend_from_slice(&[0; 12]);
    local.extend_from_slice(&(name.len() as u16).to_le_bytes());
    local.extend_from_slice(&[0; 2]);
    local.extend_from_slice(name.as_bytes());
    w.write_all(&local)?;
    *pos += local.len() as u64;
    let (mut crc, mut packed, mut size) = (0, 0, 0);
    if !dir {
        let (c, read, written) = deflate_stream(&mut std::fs::File::open(path)?, w)?;
        (crc, packed, size) = (c, zip_u32(written)?, zip_u32(read)?);
        let mut desc = Vec::with_capacity(16);
        desc.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
        for v in [crc, packed, size] {
            desc.extend_from_slice(&v.to_le_bytes());
        }
        w.write_all(&desc)?;
        *pos += written + desc.len() as u64;
    }
    entries.push(ZipEntry { name: name.clone(), dir, time, crc, packed, size, offset });
    if dir {
        let mut children: Vec<std::path::PathBuf> =
            std::fs::read_dir(path)?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>()?;
        children.sort();
        for c in children {
            let child = format!("{name}{}", c.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            zip_add(w, pos, entries, &c, child)?;
        }
    }
    Ok(())
}

/// Raw-deflates `src` into `out`: (CRC-32 of the input, bytes read, bytes written).
fn deflate_stream(src: &mut impl std::io::Read, out: &mut impl std::io::Write) -> anyhow::Result<(u32, u64, u64)> {
    use miniz_oxide::deflate::core::{compress, create_comp_flags_from_zip_params, CompressorOxide, TDEFLFlush, TDEFLStatus};
    // Negative window bits: a raw stream, without the zlib header.
    let mut comp = CompressorOxide::new(create_comp_flags_from_zip_params(6, -15, 0));
    let (mut inbuf, mut outbuf) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    let (mut crc, mut read, mut written) = (!0u32, 0u64, 0u64);
    loop {
        let n = src.read(&mut inbuf)?;
        let flush = if n == 0 { TDEFLFlush::Finish } else { TDEFLFlush::None };
        crc = crc32_update(crc, &inbuf[..n]);
        read += n as u64;
        let mut input = &inbuf[..n];
        loop {
            let (status, used, made) = compress(&mut comp, input, &mut outbuf, flush);
            out.write_all(&outbuf[..made])?;
            written += made as u64;
            input = &input[used..];
            match status {
                TDEFLStatus::Done => return Ok((!crc, read, written)),
                TDEFLStatus::Okay => {}
                _ => anyhow::bail!("deflate failed"),
            }
            // Everything taken and room to spare: on to the next chunk.
            if n > 0 && input.is_empty() && made < outbuf.len() {
                break;
            }
        }
    }
}

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *v = c;
        }
        t
    });
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// A modification time as MS-DOS (time, date), in local time.
fn dos_time(t: std::time::SystemTime) -> (u16, u16) {
    use chrono::{Datelike, Timelike};
    let dt: chrono::DateTime<chrono::Local> = t.into();
    let year = dt.year().clamp(1980, 2107) as u16;
    let date = ((year - 1980) << 9) | ((dt.month() as u16) << 5) | dt.day() as u16;
    let time = ((dt.hour() as u16) << 11) | ((dt.minute() as u16) << 5) | (dt.second() as u16 / 2);
    (time, date)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huffman_is_complete_and_limited() {
        let counts: Vec<u32> = (0..280).map(|i| if i < 256 { 1 + (i * i) as u32 } else { 0 }).collect();
        let l = huffman_lengths(&counts, 15);
        assert!(l.iter().all(|&l| l <= 15));
        let kraft: f64 = l.iter().filter(|&&l| l > 0).map(|&l| 0.5f64.powi(l as i32)).sum();
        assert!((kraft - 1.0).abs() < 1e-9);
        // Fibonacci counts would need deep codes; the limit still holds.
        let mut fib = vec![1u32, 1];
        while fib.len() < 30 {
            let n = fib[fib.len() - 1] + fib[fib.len() - 2];
            fib.push(n);
        }
        assert!(huffman_lengths(&fib, 7).iter().all(|&l| l <= 7));
    }

    #[test]
    fn webp_header() {
        let px = vec![255u8; 3 * 2 * 4];
        let f = webp(&px, 3, 2).unwrap();
        assert_eq!(&f[..4], b"RIFF");
        assert_eq!(&f[8..16], b"WEBPVP8L");
        assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()) as usize, f.len() - 8);
        assert_eq!(f[20], 0x2f);
        assert!(webp(&px, 0, 2).is_none());
    }

    #[test]
    fn ico_directory() {
        let f = ico(&[(16, vec![1, 2, 3]), (256, vec![4, 5])]);
        assert_eq!(&f[..6], &[0, 0, 1, 0, 2, 0]);
        assert_eq!(f[6], 16);
        assert_eq!(f[22], 0);
        assert_eq!(u32::from_le_bytes(f[18..22].try_into().unwrap()), 38);
        assert_eq!(&f[38..], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn crc32_known() {
        assert_eq!(!crc32_update(!0, b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn zip_round_trip() {
        let dir = std::env::temp_dir().join(format!("dockside-zip-test-{}", std::process::id()));
        let sub = dir.join("fold er");
        std::fs::create_dir_all(&sub).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("日本 ✓.txt"), "héllo").unwrap();
        std::fs::write(sub.join("big.bin"), &big).unwrap();
        let out = dir.join("out.zip");
        zip(&[dir.join("日本 ✓.txt"), sub.clone()], &out).unwrap();
        let z = std::fs::read(&out).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let u16_at = |i: usize| u16::from_le_bytes(z[i..i + 2].try_into().unwrap()) as usize;
        let u32_at = |i: usize| u32::from_le_bytes(z[i..i + 4].try_into().unwrap()) as usize;
        let end = z.len() - 22;
        assert_eq!(u32_at(end), 0x0605_4b50);
        assert_eq!(u16_at(end + 10), 3);
        let mut at = u32_at(end + 16);
        let mut found = Vec::new();
        for _ in 0..3 {
            assert_eq!(u32_at(at), 0x0201_4b50);
            let (method, crc, packed, size) = (u16_at(at + 10), u32_at(at + 16), u32_at(at + 20), u32_at(at + 24));
            let name_len = u16_at(at + 28);
            let name = String::from_utf8(z[at + 46..at + 46 + name_len].to_vec()).unwrap();
            let local = u32_at(at + 42);
            assert_eq!(u32_at(local), 0x0403_4b50);
            let data = local + 30 + u16_at(local + 26);
            let raw = if method == 8 { miniz_oxide::inflate::decompress_to_vec(&z[data..data + packed]).unwrap() } else { Vec::new() };
            assert_eq!(raw.len(), size);
            assert_eq!(if method == 8 { !crc32_update(!0, &raw) as usize } else { 0 }, crc);
            found.push((name, raw));
            at += 46 + name_len;
        }
        assert_eq!(found[0], ("日本 ✓.txt".to_string(), "héllo".as_bytes().to_vec()));
        assert_eq!(found[1].0, "fold er/");
        assert_eq!(found[2], ("fold er/big.bin".to_string(), big));
    }

    #[test]
    fn pdf_text_pages() {
        let text = (0..130).map(|i| format!("line {i} (x)")).collect::<Vec<_>>().join("\n");
        let bytes = pdf_text(&text);
        let pdf = String::from_utf8_lossy(&bytes).into_owned();
        assert!(pdf.starts_with("%PDF-1.4"));
        assert!(pdf.contains("/Count 3"));
        assert!(pdf.contains("(line 0 \\(x\\)) '"));
        assert!(pdf.trim_end().ends_with("%%EOF"));
        let xref: usize = pdf.split("startxref\n").nth(1).unwrap().lines().next().unwrap().parse().unwrap();
        assert!(bytes[xref..].starts_with(b"xref"));
    }
}
