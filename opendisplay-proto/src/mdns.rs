//! A minimal mDNS (RFC 6762) and DNS-SD (RFC 6763) responder for one service,
//! `_opendisplay._tcp`, without I/O.
//!
//! IDF 5's mDNS is a separate managed component costing 15-30 KB of flash, and
//! the firmware has about 24 KB left, so this answers exactly what discovery
//! needs and nothing else: service enumeration, the PTR for the service type,
//! the instance's SRV and TXT, and the host's A record. There is no probing or
//! conflict resolution; the names are derived from the chip's MAC, which is
//! unique on the link.
//!
//! [`respond`] turns a received query into a response; [`announce`] builds the
//! unsolicited announcement (and, with a zero TTL, the goodbye). All records go
//! in the answer section, which every resolver accepts.

/// The mDNS UDP port.
pub const PORT: u16 = 5353;
/// The IPv4 mDNS multicast group.
pub const GROUP: [u8; 4] = [224, 0, 0, 251];

/// The DNS-SD service type this responder answers for.
pub const SERVICE: &str = "_opendisplay._tcp.local";
const META: &str = "_services._dns-sd._udp.local";

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
/// Set on unique records: receivers replace what they cached for the name.
const CACHE_FLUSH: u16 = 0x8000;
/// Host records live 120 s, the rest 75 minutes (RFC 6762 section 10).
const TTL_HOST: u32 = 120;
const TTL_OTHER: u32 = 4500;
/// Answers to a legacy (non-5353) querier must not be cached for long.
const TTL_LEGACY: u32 = 10;

const FLAG_RESPONSE: u16 = 0x8000;
const FLAG_AUTHORITATIVE: u16 = 0x0400;
const OPCODE_MASK: u16 = 0x7800;
const HEADER_LEN: usize = 12;
const MAX_NAME: usize = 255;

/// What this device advertises.
#[derive(Clone, Copy, Debug)]
pub struct Service<'a> {
    /// Instance label, `<instance>._opendisplay._tcp.local`. No dots.
    pub instance: &'a str,
    /// Host label, `<host>.local`. No dots.
    pub host: &'a str,
    /// TCP port in the SRV record.
    pub port: u16,
    /// IPv4 address in the A record.
    pub address: [u8; 4],
    /// TXT strings, each `key=value`, at most 255 bytes.
    pub txt: &'a [&'a str],
}

/// The output buffer is too small.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferFull;

/// Which records a query asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Wanted {
    meta: bool,
    ptr: bool,
    srv: bool,
    txt: bool,
    a: bool,
}

impl Wanted {
    fn any(self) -> bool {
        self.meta || self.ptr || self.srv || self.txt || self.a
    }
}

/// Build the response to `query`, if it asks about this service.
///
/// `legacy` is true when the query came from a port other than 5353: such a
/// querier is a plain DNS resolver, so the reply goes back to it alone, repeats
/// its id and question, and uses short TTLs without cache-flush bits.
pub fn respond(query: &[u8], service: &Service<'_>, legacy: bool, out: &mut [u8]) -> Option<usize> {
    let header = query.get(..HEADER_LEN)?;
    let flags = u16::from_be_bytes([header[2], header[3]]);
    if flags & (FLAG_RESPONSE | OPCODE_MASK) != 0 {
        return None;
    }
    let questions = u16::from_be_bytes([header[4], header[5]]);
    let mut pos = HEADER_LEN;
    let mut wanted = Wanted::default();
    let mut first: Option<([u8; MAX_NAME], usize, u16)> = None;
    for _ in 0..questions {
        let mut name = [0u8; MAX_NAME];
        let (len, next) = read_name(query, pos, &mut name)?;
        let fixed = query.get(next..next + 4)?;
        let qtype = u16::from_be_bytes([fixed[0], fixed[1]]);
        pos = next + 4;
        let before = wanted;
        ask(&name[..len], qtype, service, &mut wanted);
        if first.is_none() && wanted != before {
            first = Some((name, len, qtype));
        }
    }
    if !wanted.any() {
        return None;
    }
    let mut writer = Writer { buf: out, len: 0 };
    let id = if legacy {
        [header[0], header[1]]
    } else {
        [0, 0]
    };
    let ttl = legacy.then_some(TTL_LEGACY);
    let records = count(wanted);
    let echo = legacy.then_some(()).and(first);
    writer.bytes(&id).ok()?;
    writer.u16(FLAG_RESPONSE | FLAG_AUTHORITATIVE).ok()?;
    writer.u16(u16::from(echo.is_some())).ok()?;
    writer.u16(records).ok()?;
    writer.u16(0).ok()?;
    writer.u16(0).ok()?;
    if let Some((name, len, qtype)) = echo {
        let name = core::str::from_utf8(&name[..len]).ok()?;
        writer.name(&[name]).ok()?;
        writer.u16(qtype).ok()?;
        writer.u16(CLASS_IN).ok()?;
    }
    records_into(&mut writer, service, wanted, ttl, !legacy).ok()?;
    Some(writer.len)
}

/// An unsolicited announcement of every record. `goodbye` sets all TTLs to
/// zero, telling caches to drop the service (sent when it stops).
pub fn announce(service: &Service<'_>, goodbye: bool, out: &mut [u8]) -> Result<usize, BufferFull> {
    let wanted = Wanted {
        meta: false,
        ptr: true,
        srv: true,
        txt: true,
        a: true,
    };
    let mut writer = Writer { buf: out, len: 0 };
    writer.u16(0)?;
    writer.u16(FLAG_RESPONSE | FLAG_AUTHORITATIVE)?;
    writer.u16(0)?;
    writer.u16(count(wanted))?;
    writer.u16(0)?;
    writer.u16(0)?;
    records_into(&mut writer, service, wanted, goodbye.then_some(0), true)?;
    Ok(writer.len)
}

/// Record what one question asks of this service.
fn ask(name: &[u8], qtype: u16, service: &Service<'_>, wanted: &mut Wanted) {
    let is = |want: u16| qtype == want || qtype == TYPE_ANY;
    if name_is(name, &[META]) && is(TYPE_PTR) {
        wanted.meta = true;
    } else if name_is(name, &[SERVICE]) && is(TYPE_PTR) {
        // A browser asking for the type gets everything needed to connect,
        // saving it the follow-up queries.
        *wanted = Wanted {
            ptr: true,
            srv: true,
            txt: true,
            a: true,
            ..*wanted
        };
    } else if name_is(name, &[service.instance, SERVICE]) {
        if is(TYPE_SRV) {
            wanted.srv = true;
            wanted.a = true;
        }
        if is(TYPE_TXT) {
            wanted.txt = true;
        }
    } else if name_is(name, &[service.host, "local"]) && is(TYPE_A) {
        wanted.a = true;
    }
}

fn count(wanted: Wanted) -> u16 {
    [wanted.meta, wanted.ptr, wanted.srv, wanted.txt, wanted.a]
        .into_iter()
        .map(u16::from)
        .sum()
}

fn records_into(
    writer: &mut Writer<'_>,
    service: &Service<'_>,
    wanted: Wanted,
    ttl: Option<u32>,
    flush: bool,
) -> Result<(), BufferFull> {
    let instance: &[&str] = &[service.instance, SERVICE];
    let host: &[&str] = &[service.host, "local"];
    let unique = CLASS_IN | if flush { CACHE_FLUSH } else { 0 };
    if wanted.meta {
        let at = writer.record(&[META], TYPE_PTR, CLASS_IN, ttl.unwrap_or(TTL_OTHER))?;
        writer.name(&[SERVICE])?;
        writer.finish_record(at)?;
    }
    if wanted.ptr {
        let at = writer.record(&[SERVICE], TYPE_PTR, CLASS_IN, ttl.unwrap_or(TTL_OTHER))?;
        writer.name(instance)?;
        writer.finish_record(at)?;
    }
    if wanted.srv {
        let at = writer.record(instance, TYPE_SRV, unique, ttl.unwrap_or(TTL_HOST))?;
        writer.u16(0)?;
        writer.u16(0)?;
        writer.u16(service.port)?;
        writer.name(host)?;
        writer.finish_record(at)?;
    }
    if wanted.txt {
        let at = writer.record(instance, TYPE_TXT, unique, ttl.unwrap_or(TTL_OTHER))?;
        if service.txt.is_empty() {
            writer.bytes(&[0])?;
        }
        for entry in service.txt {
            let len = u8::try_from(entry.len()).map_err(|_| BufferFull)?;
            writer.bytes(&[len])?;
            writer.bytes(entry.as_bytes())?;
        }
        writer.finish_record(at)?;
    }
    if wanted.a {
        let at = writer.record(host, TYPE_A, unique, ttl.unwrap_or(TTL_HOST))?;
        writer.bytes(&service.address)?;
        writer.finish_record(at)?;
    }
    Ok(())
}

/// Read a possibly compressed name at `pos` into `out` as lowercase dotted
/// text. Returns its length and the position after the name.
fn read_name(msg: &[u8], mut pos: usize, out: &mut [u8; MAX_NAME]) -> Option<(usize, usize)> {
    let mut len = 0;
    let mut resume = None;
    let mut jumps = 0;
    loop {
        let byte = *msg.get(pos)?;
        match byte & 0xC0 {
            0x00 if byte == 0 => {
                return Some((len, resume.unwrap_or(pos + 1)));
            }
            0x00 => {
                let label = msg.get(pos + 1..pos + 1 + usize::from(byte))?;
                let dot = usize::from(len > 0);
                let end = len + dot + label.len();
                if end > MAX_NAME {
                    return None;
                }
                if dot == 1 {
                    out[len] = b'.';
                }
                for (slot, byte) in out[len + dot..end].iter_mut().zip(label) {
                    *slot = byte.to_ascii_lowercase();
                }
                len = end;
                pos += 1 + label.len();
            }
            0xC0 => {
                let low = *msg.get(pos + 1)?;
                resume.get_or_insert(pos + 2);
                jumps += 1;
                // A pointer loop is a malformed packet, not a reason to spin.
                if jumps > 16 {
                    return None;
                }
                pos = usize::from(byte & 0x3F) << 8 | usize::from(low);
            }
            _ => return None,
        }
    }
}

/// True if the lowercase dotted `name` equals the parts joined with dots,
/// ignoring ASCII case.
fn name_is(name: &[u8], parts: &[&str]) -> bool {
    let mut rest = name;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            let Some(tail) = rest.strip_prefix(b".") else {
                return false;
            };
            rest = tail;
        }
        let Some((head, tail)) = rest.split_at_checked(part.len()) else {
            return false;
        };
        if !head.eq_ignore_ascii_case(part.as_bytes()) {
            return false;
        }
        rest = tail;
    }
    rest.is_empty()
}

struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Writer<'_> {
    fn bytes(&mut self, bytes: &[u8]) -> Result<(), BufferFull> {
        let end = self.len + bytes.len();
        self.buf
            .get_mut(self.len..end)
            .ok_or(BufferFull)?
            .copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    fn u16(&mut self, value: u16) -> Result<(), BufferFull> {
        self.bytes(&value.to_be_bytes())
    }

    /// Write the parts as one uncompressed name. Each part may itself hold
    /// several dot-separated labels.
    fn name(&mut self, parts: &[&str]) -> Result<(), BufferFull> {
        for label in parts.iter().flat_map(|part| part.split('.')) {
            let len = u8::try_from(label.len())
                .ok()
                .filter(|len| (1..=63).contains(len))
                .ok_or(BufferFull)?;
            self.bytes(&[len])?;
            self.bytes(label.as_bytes())?;
        }
        self.bytes(&[0])
    }

    /// Write a record header up to the data length; returns where the length
    /// goes, for [`finish_record`](Self::finish_record).
    fn record(
        &mut self,
        name: &[&str],
        kind: u16,
        class: u16,
        ttl: u32,
    ) -> Result<usize, BufferFull> {
        self.name(name)?;
        self.u16(kind)?;
        self.u16(class)?;
        self.bytes(&ttl.to_be_bytes())?;
        let at = self.len;
        self.u16(0)?;
        Ok(at)
    }

    fn finish_record(&mut self, at: usize) -> Result<(), BufferFull> {
        let len = u16::try_from(self.len - at - 2).map_err(|_| BufferFull)?;
        self.buf[at..at + 2].copy_from_slice(&len.to_be_bytes());
        Ok(())
    }
}

// ---- client side: browsing for frames ----------------------------------------

/// A one-shot browse for [`SERVICE`]: a PTR question with a non-zero id, to be
/// sent from an ordinary (non-5353) port to the group. That makes it a "legacy"
/// query (RFC 6762 section 6.7): responders answer it directly to the sender,
/// with the SRV, TXT and A records alongside, so a browser needs neither port
/// 5353 nor a resident daemon.
pub fn browse_query(id: u16, out: &mut [u8]) -> Result<usize, BufferFull> {
    let mut writer = Writer { buf: out, len: 0 };
    writer.u16(id)?;
    writer.u16(0)?;
    writer.u16(1)?;
    writer.u16(0)?;
    writer.u16(0)?;
    writer.u16(0)?;
    writer.name(&[SERVICE])?;
    writer.u16(TYPE_PTR)?;
    writer.u16(CLASS_IN)?;
    Ok(writer.len)
}

/// A decoded name, lowercase and dotted, without the trailing dot.
#[derive(Clone, Copy)]
pub struct Name {
    bytes: [u8; MAX_NAME],
    len: usize,
}

impl Name {
    /// The name's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// The name as text, if it is UTF-8 (instance names may be).
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(self.as_bytes()).ok()
    }
}

impl core::fmt::Debug for Name {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_str().unwrap_or("<non-UTF-8>"))
    }
}

/// One resource record from a response, as far as browsing needs it: `name` is
/// the record's owner, the other fields its data.
#[derive(Debug, Clone, Copy)]
#[allow(missing_docs)]
pub enum Record<'a> {
    Ptr {
        name: Name,
        target: Name,
    },
    Srv {
        name: Name,
        port: u16,
        target: Name,
    },
    /// Raw TXT data; [`txt_entries`] splits it.
    Txt {
        name: Name,
        data: &'a [u8],
    },
    A {
        name: Name,
        address: [u8; 4],
    },
}

/// Call `each` for every PTR, SRV, TXT and A record in a response (answer,
/// authority and additional sections), skipping goodbyes (TTL 0) and other
/// types. `None` when the packet is not a well-formed response.
pub fn parse_response<'a>(packet: &'a [u8], mut each: impl FnMut(Record<'a>)) -> Option<()> {
    let header = packet.get(..HEADER_LEN)?;
    let flags = u16::from_be_bytes([header[2], header[3]]);
    if flags & FLAG_RESPONSE == 0 {
        return None;
    }
    let count = |at: usize| u16::from_be_bytes([header[at], header[at + 1]]);
    let questions = count(4);
    let records = u32::from(count(6)) + u32::from(count(8)) + u32::from(count(10));
    let mut pos = HEADER_LEN;
    let mut name = [0u8; MAX_NAME];
    for _ in 0..questions {
        let (_, next) = read_name(packet, pos, &mut name)?;
        pos = next.checked_add(4)?;
    }
    for _ in 0..records {
        let (len, next) = read_name(packet, pos, &mut name)?;
        let fixed = packet.get(next..next + 10)?;
        let rtype = u16::from_be_bytes([fixed[0], fixed[1]]);
        let ttl = u32::from_be_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]);
        let rdlen = usize::from(u16::from_be_bytes([fixed[8], fixed[9]]));
        let start = next + 10;
        let data = packet.get(start..start + rdlen)?;
        pos = start + rdlen;
        if ttl == 0 {
            continue;
        }
        let owner = Name { bytes: name, len };
        let mut target = [0u8; MAX_NAME];
        match rtype {
            TYPE_PTR => {
                let (target_len, _) = read_name(packet, start, &mut target)?;
                each(Record::Ptr {
                    name: owner,
                    target: Name {
                        bytes: target,
                        len: target_len,
                    },
                });
            }
            TYPE_SRV if rdlen >= 7 => {
                let port = u16::from_be_bytes([data[4], data[5]]);
                let (target_len, _) = read_name(packet, start + 6, &mut target)?;
                each(Record::Srv {
                    name: owner,
                    port,
                    target: Name {
                        bytes: target,
                        len: target_len,
                    },
                });
            }
            TYPE_TXT => each(Record::Txt { name: owner, data }),
            TYPE_A if rdlen == 4 => each(Record::A {
                name: owner,
                address: [data[0], data[1], data[2], data[3]],
            }),
            _ => {}
        }
    }
    Some(())
}

/// The `key=value` entries of TXT data; an entry without `=` has an empty value.
pub fn txt_entries(data: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut rest = data;
    core::iter::from_fn(move || {
        loop {
            let (&len, tail) = rest.split_first()?;
            let (entry, after) = tail.split_at_checked(usize::from(len))?;
            rest = after;
            if entry.is_empty() {
                continue;
            }
            return Some(match entry.iter().position(|&byte| byte == b'=') {
                Some(at) => (&entry[..at], &entry[at + 1..]),
                None => (entry, &[][..]),
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICE_UNDER_TEST: Service<'static> = Service {
        instance: "ODCAED44",
        host: "ODCAED44",
        port: 2446,
        address: [192, 168, 50, 204],
        txt: &["mac=94:a9:90:ca:ed:46", "tls=0"],
    };

    /// A query with one question, as a resolver writes it.
    fn query(name: &str, qtype: u16, buf: &mut [u8; 128]) -> usize {
        let mut writer = Writer { buf, len: 0 };
        writer
            .bytes(&[0x12, 0x34, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0])
            .unwrap();
        writer.name(&[name]).unwrap();
        writer.u16(qtype).unwrap();
        writer.u16(CLASS_IN).unwrap();
        writer.len
    }

    /// (type, name) of every answer in a response.
    fn answers(msg: &[u8]) -> [(u16, [u8; MAX_NAME], usize); 5] {
        let mut out = [(0, [0; MAX_NAME], 0); 5];
        let questions = u16::from_be_bytes([msg[4], msg[5]]);
        let count = u16::from_be_bytes([msg[6], msg[7]]);
        let mut pos = HEADER_LEN;
        for _ in 0..questions {
            let mut name = [0; MAX_NAME];
            pos = read_name(msg, pos, &mut name).unwrap().1 + 4;
        }
        for slot in out.iter_mut().take(usize::from(count)) {
            let (len, next) = read_name(msg, pos, &mut slot.1).unwrap();
            slot.0 = u16::from_be_bytes([msg[next], msg[next + 1]]);
            slot.2 = len;
            let rdlen = usize::from(u16::from_be_bytes([msg[next + 8], msg[next + 9]]));
            pos = next + 10 + rdlen;
        }
        assert_eq!(pos, msg.len(), "trailing bytes");
        out
    }

    fn types(msg: &[u8]) -> [u16; 5] {
        answers(msg).map(|(kind, _, _)| kind)
    }

    #[test]
    fn browsing_the_type_returns_everything_needed_to_connect() {
        let mut q = [0u8; 128];
        let len = query("_opendisplay._tcp.local", TYPE_PTR, &mut q);
        let mut out = [0u8; 512];
        let n = respond(&q[..len], &SERVICE_UNDER_TEST, false, &mut out).unwrap();
        assert_eq!(&out[..4], &[0, 0, 0x84, 0x00]);
        assert_eq!(types(&out[..n]), [TYPE_PTR, TYPE_SRV, TYPE_TXT, TYPE_A, 0]);
        let (_, name, len) = answers(&out[..n])[1];
        assert_eq!(&name[..len], b"odcaed44._opendisplay._tcp.local");
    }

    #[test]
    fn names_match_without_regard_to_case() {
        let mut q = [0u8; 128];
        let mut out = [0u8; 512];
        let len = query("odcaed44.LOCAL", TYPE_A, &mut q);
        let n = respond(&q[..len], &SERVICE_UNDER_TEST, false, &mut out).unwrap();
        assert_eq!(types(&out[..n]), [TYPE_A, 0, 0, 0, 0]);
        assert_eq!(&out[n - 4..n], &[192, 168, 50, 204]);

        let len = query("ODCAED44._opendisplay._tcp.local", TYPE_TXT, &mut q);
        let n = respond(&q[..len], &SERVICE_UNDER_TEST, false, &mut out).unwrap();
        assert_eq!(types(&out[..n]), [TYPE_TXT, 0, 0, 0, 0]);

        let len = query("_services._dns-sd._udp.local", TYPE_PTR, &mut q);
        let n = respond(&q[..len], &SERVICE_UNDER_TEST, false, &mut out).unwrap();
        assert_eq!(types(&out[..n]), [TYPE_PTR, 0, 0, 0, 0]);
    }

    #[test]
    fn other_names_and_responses_are_ignored() {
        let mut q = [0u8; 128];
        let mut out = [0u8; 512];
        let len = query("_http._tcp.local", TYPE_PTR, &mut q);
        assert_eq!(
            respond(&q[..len], &SERVICE_UNDER_TEST, false, &mut out),
            None
        );
        let len = query("odcaed44.local", TYPE_TXT, &mut q);
        assert_eq!(
            respond(&q[..len], &SERVICE_UNDER_TEST, false, &mut out),
            None
        );
        let len = query("_opendisplay._tcp.local", TYPE_PTR, &mut q);
        q[2] = 0x84;
        assert_eq!(
            respond(&q[..len], &SERVICE_UNDER_TEST, false, &mut out),
            None
        );
        assert_eq!(respond(&q[..5], &SERVICE_UNDER_TEST, false, &mut out), None);
    }

    #[test]
    fn legacy_queries_get_their_id_question_and_short_ttls() {
        let mut q = [0u8; 128];
        let mut out = [0u8; 512];
        let len = query("odcaed44.local", TYPE_A, &mut q);
        let n = respond(&q[..len], &SERVICE_UNDER_TEST, true, &mut out).unwrap();
        assert_eq!(&out[..2], &[0x12, 0x34]);
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 1);
        // A record: class IN without cache-flush, TTL 10.
        assert_eq!(&out[n - 4 - 2 - 4 - 2..n - 4 - 2], &[0, 1, 0, 0, 0, 10]);
    }

    #[test]
    fn compressed_question_names_are_followed() {
        // Two questions; the second points back into the first name.
        let mut q = [0u8; 128];
        let mut writer = Writer {
            buf: &mut q,
            len: 0,
        };
        writer.bytes(&[0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0]).unwrap();
        writer.name(&["_opendisplay._tcp.local"]).unwrap();
        writer.u16(TYPE_PTR).unwrap();
        writer.u16(CLASS_IN).unwrap();
        writer.bytes(&[8]).unwrap();
        writer.bytes(b"ODCAED44").unwrap();
        writer.bytes(&[0xC0, HEADER_LEN as u8]).unwrap();
        writer.u16(TYPE_SRV).unwrap();
        writer.u16(CLASS_IN).unwrap();
        let len = writer.len;
        let mut out = [0u8; 512];
        let n = respond(&q[..len], &SERVICE_UNDER_TEST, false, &mut out).unwrap();
        assert_eq!(types(&out[..n]), [TYPE_PTR, TYPE_SRV, TYPE_TXT, TYPE_A, 0]);

        // A pointer to itself must not hang.
        let looped = [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0xC0, 12, 0, 12, 0, 1];
        assert_eq!(respond(&looped, &SERVICE_UNDER_TEST, false, &mut out), None);
    }

    #[test]
    fn announcement_and_goodbye() {
        let mut out = [0u8; 512];
        let n = announce(&SERVICE_UNDER_TEST, false, &mut out).unwrap();
        assert_eq!(types(&out[..n]), [TYPE_PTR, TYPE_SRV, TYPE_TXT, TYPE_A, 0]);
        let n = announce(&SERVICE_UNDER_TEST, true, &mut out).unwrap();
        // The A record is last: its TTL bytes sit just before rdlength and data.
        assert_eq!(&out[n - 10..n - 6], &[0, 0, 0, 0]);
        assert_eq!(
            announce(&SERVICE_UNDER_TEST, false, &mut out[..40]),
            Err(BufferFull)
        );
    }

    #[test]
    fn a_browse_reads_back_what_the_responder_says() {
        let mut query = [0u8; 64];
        let len = browse_query(0x2446, &mut query).unwrap();
        let service = Service {
            instance: "ODCAED44",
            host: "ODCAED44",
            port: 2446,
            address: [192, 168, 50, 204],
            txt: &["mac=94:a9:90:ca:ed:46", "tls=0", "flag"],
        };
        let mut answer = [0u8; 512];
        // The responder treats a query from a non-5353 port as legacy.
        let answer_len = respond(&query[..len], &service, true, &mut answer).unwrap();
        let (mut ptr, mut srv, mut a, mut mac, mut flag) = (None, None, None, None, false);
        parse_response(&answer[..answer_len], |record| match record {
            Record::Ptr { target, .. } => ptr = Some(target),
            Record::Srv { port, target, .. } => srv = Some((port, target)),
            Record::A { address, .. } => a = Some(address),
            Record::Txt { data, .. } => {
                for (key, value) in txt_entries(data) {
                    if key == b"mac" {
                        mac = Some(value.len());
                    }
                    flag |= key == b"flag" && value.is_empty();
                }
            }
        })
        .unwrap();
        assert_eq!(
            ptr.unwrap().as_str(),
            Some("odcaed44._opendisplay._tcp.local")
        );
        let (port, host) = srv.unwrap();
        assert_eq!((port, host.as_str()), (2446, Some("odcaed44.local")));
        assert_eq!(a, Some([192, 168, 50, 204]));
        assert_eq!(mac, Some("94:a9:90:ca:ed:46".len()));
        assert!(flag);
    }

    #[test]
    fn a_goodbye_or_a_query_yields_nothing() {
        let service = Service {
            instance: "X",
            host: "X",
            port: 1,
            address: [1, 2, 3, 4],
            txt: &[],
        };
        let mut packet = [0u8; 512];
        let len = announce(&service, true, &mut packet).unwrap();
        let mut seen = 0;
        parse_response(&packet[..len], |_| seen += 1).unwrap();
        assert_eq!(seen, 0, "TTL 0 records are goodbyes");
        let mut query = [0u8; 64];
        let len = browse_query(1, &mut query).unwrap();
        assert!(parse_response(&query[..len], |_| {}).is_none());
    }
}
