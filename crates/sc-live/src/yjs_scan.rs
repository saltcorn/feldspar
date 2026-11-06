//! A Yjs v1 update read for its shape, before `yrs` decodes it
//! (TODO-type-hardening H5.3).
//!
//! `yrs` recurses over two things an update controls, with no limit on
//! either, and a stack overflow is not a panic `catch_unwind` can stop — it
//! takes the whole server down, every application on it:
//!
//! - **a value's nesting**: an `Any` array inside an array inside an array is
//!   two bytes a level, so a one-megabyte `doc_update` is half a million
//!   levels of `Any::decode`;
//! - **the document's**: shared types (an XML element) nested in each other
//!   are walked recursively when one is deleted, garbage-collected or turned
//!   into a string.
//!
//! So an update is first read here, iteratively: every value's nesting is held
//! to [`MAX_ANY_DEPTH`] (an error otherwise), and the shared types it
//! **creates** are counted — the most it can add to the document's depth,
//! which [`crate::doc`] keeps within its own bound.
//!
//! This mirrors `yrs` 0.28's v1 decoder (`Update::decode`, `Item` content,
//! `TypeRef`, `Any::decode`) field for field, quirks included (a JSON content
//! of length `n` is read as `n + 1` strings, as `yrs` reads it): a byte this
//! accepts and `yrs` does not, or the other way round, is a refusal of a
//! valid update or a value read unchecked. `tests/doc_format.rs` replays
//! updates recorded from the JavaScript package through it.

use yrs::{ClientID, StateVector};

/// How deep one value in an update may nest (arrays and maps in each other).
/// A real document's attribute values are a level or two deep.
pub(crate) const MAX_ANY_DEPTH: usize = 64;

/// What a scan found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scan {
    /// Shared types the update creates that `known` does not hold yet.
    pub(crate) new_types: usize,
}

/// Why an update was refused by the scan.
pub(crate) type ScanError = String;

/// The client ids `yrs` keeps: 53 bits (a larger one is cut to them).
const CLIENT_BITS: u64 = (1 << 53) - 1;

/// Read `update`'s structs, counting the shared types it creates beyond
/// `known` (a document's state vector), and refusing a value nested deeper
/// than [`MAX_ANY_DEPTH`]. The delete set after the structs is flat and is not
/// read.
pub(crate) fn scan(update: &[u8], known: &StateVector) -> Result<Scan, ScanError> {
    let mut new_types = 0_usize;
    each_struct(update, |s| {
        if matches!(s.item, Some((_, true))) && s.id.1 >= known.get(&ClientID::new(s.id.0)) {
            new_types = new_types.saturating_add(1);
        }
    })?;
    Ok(Scan { new_types })
}

/// An item's id: its client (cut to the bits `yrs` keeps) and clock.
type Id = (u64, u32);

/// Where an item says its parent is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parent {
    /// A root type, by name.
    Root,
    /// The shared type that the item with this id holds.
    Item(Id),
    /// The parent of the item it was inserted next to (its left origin, or
    /// else its right one): the encoding leaves it out.
    Beside(Id),
}

/// One struct of an update, as far as nesting goes.
#[derive(Debug, Clone, Copy)]
struct Shape {
    id: Id,
    len: u32,
    /// `None` for a range that was garbage-collected (still clocks the
    /// document holds); an item has a parent and is or is not a shared type.
    item: Option<(Parent, bool)>,
}

/// Call `f` with every item and garbage-collected range of `update`, in
/// order. Skips — clocks the update does not hold — are passed over.
fn each_struct(update: &[u8], mut f: impl FnMut(Shape)) -> Result<(), ScanError> {
    let mut r = Reader {
        bytes: update,
        pos: 0,
    };
    let clients = r.var_u32()?;
    for _ in 0..clients {
        let blocks = r.var_u32()?;
        let client = r.var_u64()? & CLIENT_BITS;
        let mut clock = r.var_u32()?;
        for _ in 0..blocks {
            let (len, skip, item) = r.block()?;
            if len > 0 && !skip {
                f(Shape {
                    id: (client, clock),
                    len,
                    item,
                });
            }
            clock = clock.wrapping_add(len);
        }
    }
    Ok(())
}

/// One item, for [`depth`].
#[derive(Debug, Clone, Copy)]
struct Node {
    id: Id,
    len: u32,
    parent: Parent,
    is_type: bool,
}

/// How deep the shared types of a document nest, a root being one level:
/// `state` (what the document holds, as `encode_state_as_update` writes it)
/// with `more` (its pending update, an update about to be applied) on top.
///
/// Read from the encodings, not from a `yrs` document: the point is to know
/// before `yrs` holds a tree that deep, and `yrs`'s own iterators are not
/// safe on every tree a client can make. As `yrs` applies them, what `state`
/// already holds is not taken again from `more` (an item that overlaps it
/// keeps only its part past it). An item's parent is the one it names, or
/// else the parent of the item it was inserted beside. Iterative throughout;
/// an item whose parent is not there (it is waiting for it) counts from a
/// root, and a cycle — which `yrs` never lets in — counts once.
pub(crate) fn depth(state: &[u8], more: &[&[u8]]) -> Result<usize, ScanError> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut held: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    each_struct(state, |s| {
        let end = held.entry(s.id.0).or_insert(0);
        *end = (*end).max(s.id.1.saturating_add(s.len));
        if let Some((parent, is_type)) = s.item {
            nodes.push(Node {
                id: s.id,
                len: s.len,
                parent,
                is_type,
            });
        }
    })?;
    for part in more {
        each_struct(part, |s| {
            let Some((parent, is_type)) = s.item else {
                return;
            };
            let known = held.get(&s.id.0).copied().unwrap_or(0);
            let end = s.id.1.saturating_add(s.len);
            if end <= known {
                return;
            }
            let start = s.id.1.max(known);
            nodes.push(Node {
                id: (s.id.0, start),
                len: end - start,
                parent,
                is_type,
            });
        })?;
    }
    // By client, then clock; the first of two with one id is kept (the
    // state's, or the earlier part's).
    nodes.sort_by_key(|n| n.id);
    nodes.dedup_by_key(|n| n.id);
    // For each position, the node reaching furthest among those of its client
    // up to it: the one an id falls in, if any does.
    let mut reach: Vec<(u64, usize)> = Vec::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        let end = u64::from(n.id.1) + u64::from(n.len);
        let best = match (i.checked_sub(1), reach.last()) {
            (Some(prev), Some(&(prev_end, at)))
                if nodes[prev].id.0 == n.id.0 && prev_end >= end =>
            {
                (prev_end, at)
            }
            _ => (end, i),
        };
        reach.push(best);
    }
    let find = |id: Id| -> Option<usize> {
        let at = nodes.partition_point(|n| n.id <= id).checked_sub(1)?;
        let (end, holder) = reach[at];
        (nodes[holder].id.0 == id.0 && end > u64::from(id.1)).then_some(holder)
    };

    // The type each node's parent is: `Some(index)`, or `None` for a root (or
    // a parent not found).
    #[derive(Clone, Copy)]
    enum Resolved {
        Pending,
        Visiting,
        Done(Option<usize>),
    }
    let mut parent: Vec<Resolved> = vec![Resolved::Pending; nodes.len()];
    for start in 0..nodes.len() {
        let mut chain = Vec::new();
        let mut at = start;
        let found = loop {
            match parent[at] {
                Resolved::Done(p) => break p,
                Resolved::Visiting => break None,
                Resolved::Pending => {}
            }
            parent[at] = Resolved::Visiting;
            chain.push(at);
            match nodes[at].parent {
                Parent::Root => break None,
                Parent::Item(id) => break find(id).filter(|&p| nodes[p].is_type),
                Parent::Beside(id) => match find(id) {
                    Some(next) => at = next,
                    None => break None,
                },
            }
        };
        for i in chain {
            parent[i] = Resolved::Done(found);
        }
    }

    // Each type's depth: one below its parent's; a root's children are at two.
    const ON_CHAIN: usize = usize::MAX;
    let mut depth: Vec<usize> = vec![0; nodes.len()];
    let mut deepest = 1;
    for start in 0..nodes.len() {
        if !nodes[start].is_type || depth[start] != 0 {
            continue;
        }
        let mut chain = Vec::new();
        let mut at = Some(start);
        let base = loop {
            let Some(i) = at else { break 1 };
            match depth[i] {
                0 => {}
                ON_CHAIN => break 1,
                known => break known,
            }
            depth[i] = ON_CHAIN;
            chain.push(i);
            at = match parent[i] {
                Resolved::Done(p) => p,
                _ => None,
            };
        };
        for (n, i) in chain.into_iter().rev().enumerate() {
            depth[i] = base.saturating_add(n).saturating_add(1);
            deepest = deepest.max(depth[i]);
        }
    }
    Ok(deepest)
}

// The info byte (`yrs::block`).
const BLOCK_GC: u8 = 0;
const BLOCK_SKIP: u8 = 10;
const HAS_ORIGIN: u8 = 0b1000_0000;
const HAS_RIGHT_ORIGIN: u8 = 0b0100_0000;
const HAS_PARENT_SUB: u8 = 0b0010_0000;
const CONTENT_MASK: u8 = 0b1111;

// Content refs.
const CONTENT_DELETED: u8 = 1;
const CONTENT_JSON: u8 = 2;
const CONTENT_BINARY: u8 = 3;
const CONTENT_STRING: u8 = 4;
const CONTENT_EMBED: u8 = 5;
const CONTENT_FORMAT: u8 = 6;
const CONTENT_TYPE: u8 = 7;
const CONTENT_ANY: u8 = 8;
const CONTENT_DOC: u8 = 9;

// Type refs (`yrs::types`); 7, a weak link, is not compiled into `yrs` here.
const TYPE_XML_ELEMENT: u8 = 3;
const TYPE_REFS_PLAIN: [u8; 8] = [0, 1, 2, 4, 5, 6, 9, 15];

/// What [`Reader::block`] read: the clock length, whether it is a skip, and
/// for an item its parent and whether it is a shared type.
type Block = (u32, bool, Option<(Parent, bool)>);

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

/// One open array or map while a value is read.
struct Open {
    remaining: u64,
    is_map: bool,
}

impl Reader<'_> {
    fn ended() -> ScanError {
        "the update ends in the middle of a value".to_owned()
    }

    fn u8(&mut self) -> Result<u8, ScanError> {
        let b = *self.bytes.get(self.pos).ok_or_else(Self::ended)?;
        self.pos += 1;
        Ok(b)
    }

    fn skip(&mut self, n: u64) -> Result<&[u8], ScanError> {
        let n = usize::try_from(n).map_err(|_| Self::ended())?;
        let end = self.pos.checked_add(n).ok_or_else(Self::ended)?;
        let slice = self.bytes.get(self.pos..end).ok_or_else(Self::ended)?;
        self.pos = end;
        Ok(slice)
    }

    /// A lib0 unsigned varint, as `yrs` reads one: up to 11 bytes.
    fn var_u64(&mut self) -> Result<u64, ScanError> {
        let mut num = 0_u64;
        let mut shift = 0_u32;
        loop {
            let b = self.u8()?;
            num |= u64::from(b & 0x7f).wrapping_shl(shift);
            shift += 7;
            if b < 0x80 {
                return Ok(num);
            }
            if shift > 70 {
                return Err("an integer in the update is too long".to_owned());
            }
        }
    }

    /// The same read into 32 bits, wrapping as `yrs` does.
    fn var_u32(&mut self) -> Result<u32, ScanError> {
        let mut num = 0_u32;
        let mut shift = 0_u32;
        loop {
            let b = self.u8()?;
            num |= u32::from(b & 0x7f).wrapping_shl(shift);
            shift += 7;
            if b < 0x80 {
                return Ok(num);
            }
            if shift > 70 {
                return Err("an integer in the update is too long".to_owned());
            }
        }
    }

    /// A lib0 signed varint: the bytes, not the value.
    fn skip_var_i64(&mut self) -> Result<(), ScanError> {
        let mut b = self.u8()?;
        let mut shift = 6_u32;
        while b >= 0x80 {
            b = self.u8()?;
            shift += 7;
            if b >= 0x80 && shift > 70 {
                return Err("an integer in the update is too long".to_owned());
            }
        }
        Ok(())
    }

    fn buf(&mut self) -> Result<&[u8], ScanError> {
        let len = self.var_u32()?;
        self.skip(u64::from(len))
    }

    fn string(&mut self) -> Result<&str, ScanError> {
        std::str::from_utf8(self.buf()?)
            .map_err(|_| "a string in the update is not UTF-8".to_owned())
    }

    fn id(&mut self) -> Result<Id, ScanError> {
        let client = self.var_u64()? & CLIENT_BITS;
        Ok((client, self.var_u32()?))
    }

    /// One struct: its clock length, whether it is a skip, and for an item
    /// where its parent is and whether it creates a shared type.
    fn block(&mut self) -> Result<Block, ScanError> {
        let info = self.u8()?;
        if info == BLOCK_SKIP {
            return Ok((self.var_u32()?, true, None));
        }
        if info == BLOCK_GC {
            return Ok((self.var_u32()?, false, None));
        }
        let cant_copy_parent_info = info & (HAS_ORIGIN | HAS_RIGHT_ORIGIN) == 0;
        let origin = if info & HAS_ORIGIN != 0 {
            Some(self.id()?)
        } else {
            None
        };
        let right_origin = if info & HAS_RIGHT_ORIGIN != 0 {
            Some(self.id()?)
        } else {
            None
        };
        let parent = if cant_copy_parent_info {
            let parent = if self.var_u32()? == 1 {
                self.string()?;
                Parent::Root
            } else {
                Parent::Item(self.id()?)
            };
            if info & HAS_PARENT_SUB != 0 {
                self.string()?;
            }
            parent
        } else {
            match (origin, right_origin) {
                (Some(id), _) | (None, Some(id)) => Parent::Beside(id),
                (None, None) => Parent::Root,
            }
        };
        let (len, is_type) = self.content(info)?;
        Ok((len, false, Some((parent, is_type))))
    }

    /// An item's content: its clock length, and whether it is a shared type.
    fn content(&mut self, info: u8) -> Result<(u32, bool), ScanError> {
        Ok(match info & CONTENT_MASK {
            CONTENT_DELETED => (self.var_u32()?, false),
            CONTENT_JSON => {
                // `yrs` reads one more string than the length says.
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "mirrors yrs, which reads the length as an i32"
                )]
                let mut remaining = self.var_u32()? as i32;
                let mut count = 0_u32;
                while remaining >= 0 {
                    self.string()?;
                    remaining -= 1;
                    count = count.wrapping_add(1);
                }
                (count, false)
            }
            CONTENT_BINARY => {
                self.buf()?;
                (1, false)
            }
            CONTENT_STRING => {
                let len = self.string()?.encode_utf16().count();
                (u32::try_from(len).unwrap_or(u32::MAX), false)
            }
            CONTENT_EMBED => {
                self.string()?;
                (1, false)
            }
            CONTENT_FORMAT => {
                self.string()?;
                self.string()?;
                (1, false)
            }
            CONTENT_TYPE => {
                let type_ref = self.u8()?;
                if type_ref == TYPE_XML_ELEMENT {
                    self.string()?;
                } else if !TYPE_REFS_PLAIN.contains(&type_ref) {
                    return Err(format!("{type_ref} is not a shared type"));
                }
                (1, true)
            }
            CONTENT_ANY => {
                let n = self.var_u32()?;
                for _ in 0..n {
                    self.any()?;
                }
                (n, false)
            }
            CONTENT_DOC => {
                self.string()?;
                self.any()?;
                (1, false)
            }
            other => return Err(format!("{other} is not a content type")),
        })
    }

    /// One `Any`, with an explicit stack of the arrays and maps it is inside,
    /// refused past [`MAX_ANY_DEPTH`].
    fn any(&mut self) -> Result<(), ScanError> {
        let mut open: Vec<Open> = Vec::new();
        let mut top = true;
        loop {
            match open.last_mut() {
                Some(frame) if frame.remaining == 0 => {
                    open.pop();
                    if open.is_empty() {
                        return Ok(());
                    }
                    continue;
                }
                Some(frame) => {
                    frame.remaining -= 1;
                    if frame.is_map {
                        self.string()?;
                    }
                }
                None if top => top = false,
                None => return Ok(()),
            }
            match self.u8()? {
                // undefined, null, false, true
                127 | 126 | 121 | 120 => {}
                125 => self.skip_var_i64()?,
                124 => {
                    self.skip(4)?;
                }
                123 | 122 => {
                    self.skip(8)?;
                }
                119 => {
                    self.string()?;
                }
                tag @ (118 | 117) => {
                    let remaining = self.var_u64()?;
                    if remaining > 0 {
                        if open.len() >= MAX_ANY_DEPTH {
                            return Err(format!(
                                "a value in the update nests more than {MAX_ANY_DEPTH} levels deep"
                            ));
                        }
                        open.push(Open {
                            remaining,
                            is_map: tag == 118,
                        });
                    }
                }
                116 => {
                    self.buf()?;
                }
                other => return Err(format!("{other} is not a value type")),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use yrs::{
        Any, Array, Doc, Map, ReadTxn, Text, Transact, WriteTxn, Xml, XmlElementPrelim,
        XmlFragment, XmlTextPrelim,
    };

    /// An update with one `Any` item under root `t`: `depth` arrays, one in
    /// the next, around a null.
    fn nested_any(depth: usize) -> Vec<u8> {
        let mut bytes = vec![1, 1, 1, 0]; // one client, one struct, client 1, clock 0
        bytes.push(CONTENT_ANY); // no origins: the parent follows
        bytes.extend([1, 1, b't']); // a root, named `t`
        bytes.push(1); // one value
        for _ in 0..depth {
            bytes.extend([117, 1]);
        }
        bytes.push(126);
        bytes.push(0); // an empty delete set
        bytes
    }

    #[test]
    fn a_value_nested_past_the_bound_is_refused_without_recursing() {
        assert!(scan(&nested_any(MAX_ANY_DEPTH), &StateVector::default()).is_ok());
        let err = scan(&nested_any(500_000), &StateVector::default()).unwrap_err();
        assert!(err.contains("nests more than 64 levels"), "{err}");
    }

    #[test]
    fn what_yrs_writes_is_read_and_its_new_types_counted() {
        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            let xml = txn.get_or_insert_xml_fragment("x");
            let p = xml.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("héllo 👋"));
            p.insert_attribute(&mut txn, "data", Any::from(vec![Any::from(1.5), Any::Null]));
            let text = txn.get_or_insert_text("t");
            text.insert(&mut txn, 0, "plain text");
            text.format(&mut txn, 0, 5, [("bold".into(), Any::Bool(true))].into());
            let map = txn.get_or_insert_map("m");
            map.insert(&mut txn, "n", -42_i64);
            map.insert(&mut txn, "big", 1_i64 << 40);
            let array = txn.get_or_insert_array("a");
            array.push_back(&mut txn, "x");
            array.push_back(&mut txn, yrs::ArrayPrelim::default());
            text.remove_range(&mut txn, 2, 3);
        }
        let state = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        // The paragraph, its text and the nested array.
        assert_eq!(scan(&state, &StateVector::default()).unwrap().new_types, 3);
        // Against itself, nothing is new.
        let known = doc.transact().state_vector();
        assert_eq!(scan(&state, &known).unwrap().new_types, 0);
        // An update that adds one element counts one.
        let before = doc.transact().state_vector();
        {
            let mut txn = doc.transact_mut();
            let xml = txn.get_or_insert_xml_fragment("x");
            xml.push_back(&mut txn, XmlElementPrelim::empty("hr"));
        }
        let update = doc.transact().encode_diff_v1(&before);
        assert_eq!(scan(&update, &before).unwrap().new_types, 1);
    }

    #[test]
    fn the_depth_of_a_document_is_read_from_its_encoding() {
        let doc = Doc::new();
        let mut before = doc.transact().state_vector();
        let mut updates = Vec::new();
        {
            let mut txn = doc.transact_mut();
            let xml = txn.get_or_insert_xml_fragment("x");
            let list = xml.push_back(&mut txn, XmlElementPrelim::empty("bulletList"));
            let item = list.push_back(&mut txn, XmlElementPrelim::empty("listItem"));
            let p = item.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("deep"));
            // A sibling inserted beside the first: its parent is not written.
            list.insert(&mut txn, 0, XmlElementPrelim::empty("listItem"));
        }
        updates.push(doc.transact().encode_diff_v1(&before));
        before = doc.transact().state_vector();
        {
            let mut txn = doc.transact_mut();
            let xml = txn.get_or_insert_xml_fragment("x");
            let first = xml.get(&txn, 0).unwrap();
            let yrs::XmlOut::Element(list) = first else {
                panic!("an element")
            };
            let added = list.get(&txn, 0).unwrap();
            let yrs::XmlOut::Element(added) = added else {
                panic!("an element")
            };
            let q = added.push_back(&mut txn, XmlElementPrelim::empty("quote"));
            let p = q.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("deeper"));
        }
        updates.push(doc.transact().encode_diff_v1(&before));
        let state = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        // x > bulletList > listItem > quote > paragraph > text: six.
        assert_eq!(depth(&state, &[]).unwrap(), 6);
        assert_eq!(depth(&updates[0], &[&updates[1]]).unwrap(), 6);
        // The first update alone: x > bulletList > listItem > paragraph > text.
        assert_eq!(depth(&updates[0], &[]).unwrap(), 5);
        // What the state already holds is not counted twice.
        assert_eq!(depth(&state, &[&updates[0], &updates[1]]).unwrap(), 6);
        assert_eq!(depth(&[0, 0], &[]).unwrap(), 1);
    }

    #[test]
    fn truncated_input_is_an_error() {
        let state = nested_any(3);
        for cut in 0..state.len() - 1 {
            assert!(
                scan(&state[..cut], &StateVector::default()).is_err(),
                "{cut}"
            );
        }
    }
}
