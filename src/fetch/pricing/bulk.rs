//! Bulk price tables: one row per instance type (RDS, ElastiCache, SageMaker).
//!
//! A bulk template is a table whose first body row holds `bulkPriceOf` and
//! `bulkStringOf` tokens. The page's script builds the region's records from the
//! sets the markup names (`data-token-sets`), keeps the records that publish a
//! value for every token, sorts them by `data-sort-property` with the order
//! `data-sort-order` names, and shows one row per record under the header row.
//! With `data-relates`, records of several sets that share an attribute (the
//! instance type) are joined into one row.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde_json::Value;

use super::Token;

/// One record: the joined value of `data-relates`, and the price-file entries
/// it carries, by `(service, source, set)`.
pub(super) struct Row<'a> {
    related: Option<String>,
    entries: Vec<((String, String, String), &'a Value)>,
}

impl<'a> Row<'a> {
    fn entry(&self, service: &str, source: &str, set: &str) -> Option<&'a Value> {
        self.entries
            .iter()
            .find(|((a, b, c), _)| a == service && b == source && c == set)
            .map(|(_, v)| *v)
    }
}

/// What the markup says about its sets and their order.
pub(super) struct Sets<'m> {
    pub sets: &'m [String],
    pub relates: Option<&'m str>,
    pub sort_property: &'m str,
    pub sort_order: &'m str,
}

/// `dir/file.[set]` or `dir/file.[set].[attribute]` split as the script splits
/// it; anything else is one opaque part.
fn parse_set(spec: &str) -> Vec<&str> {
    let parsed = (|| {
        let (dir, rest) = spec.split_once('/')?;
        if dir.is_empty() {
            return None;
        }
        let dot = rest.find('.')?;
        let (file, rest) = (&rest[..dot], &rest[dot + 1..]);
        if file.is_empty() {
            return None;
        }
        let rest = rest.strip_prefix('[')?;
        let close = rest.find(']')?;
        let (set, rest) = (&rest[..close], &rest[close + 1..]);
        if set.is_empty() {
            return None;
        }
        if rest.is_empty() {
            return Some(vec![dir, file, set]);
        }
        let attr = rest.strip_prefix(".[")?.strip_suffix(']')?;
        (!attr.is_empty() && !attr.contains(']')).then(|| vec![dir, file, set, attr])
    })();
    parsed.unwrap_or_else(|| vec![spec])
}

/// The sets of `sets` that belong to `product` (`service/source`), by name.
fn set_names<'m>(sets: &'m [String], product: &str) -> Vec<Option<&'m str>> {
    sets.iter()
        .filter(|s| s.starts_with(product))
        .map(|s| parse_set(s).get(2).copied())
        .collect()
}

fn product(file: &Value) -> Option<(String, String)> {
    let m = file.get("manifest")?;
    Some((
        m.get("serviceId")?.as_str()?.to_string(),
        m.get("source")?.as_str()?.to_string(),
    ))
}

fn units<'f>(file: &'f Value, set: &str) -> Vec<&'f str> {
    file.get("sets")
        .and_then(|s| s.get(set))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// The region's records, in the order the script holds them before sorting;
/// `None` when the script would show none (or fail).
pub(super) fn rows<'a>(files: &[&'a Value], region: &str, meta: &Sets<'_>) -> Option<Vec<Row<'a>>> {
    let rows = match meta.relates {
        None => simple(files.first()?, region, meta.sets)?,
        Some(relates) => related(files, region, meta.sets, relates)?,
    };
    (!rows.is_empty()).then_some(rows)
}

/// The first file's records of the named sets, every one filed under the first
/// set's name (as the script files them).
fn simple<'a>(file: &'a Value, region: &str, sets: &[String]) -> Option<Vec<Row<'a>>> {
    let (service, source) = product(file)?;
    let names = set_names(sets, &format!("{service}/{source}"));
    let all: Vec<&str> = names
        .iter()
        .flatten()
        .flat_map(|n| units(file, n))
        .collect();
    let here = file.get("regions")?.get(region)?.as_object()?;
    let first = (*names.first()?)?;
    Some(
        all.iter()
            .filter_map(|u| here.get(*u))
            .map(|entry| Row {
                related: None,
                entries: vec![((service.clone(), source.clone(), first.to_string()), entry)],
            })
            .collect(),
    )
}

/// Records of every file's named sets joined on the attribute `relates` names
/// for each set: one row per distinct value, in the order an object's keys
/// enumerate (integer-like keys first, ascending; the rest as first seen).
fn related<'a>(
    files: &[&'a Value],
    region: &str,
    sets: &[String],
    relates: &str,
) -> Option<Vec<Row<'a>>> {
    let mut order: Vec<String> = Vec::new();
    let mut joined: HashMap<String, Row<'a>> = HashMap::new();
    for file in files {
        let (service, source) = product(file)?;
        let prefix = format!("{service}/{source}");
        let names = set_names(sets, &prefix);
        let Some(here) = file
            .get("regions")
            .and_then(|r| r.get(region))
            .and_then(Value::as_object)
        else {
            continue;
        };
        let in_region = names
            .iter()
            .flatten()
            .flat_map(|n| units(file, n))
            .any(|u| here.contains_key(u));
        if !in_region {
            continue;
        }
        for name in &names {
            let name = (*name)?;
            let spec = format!("{prefix}.[{name}]");
            let link = relates.split(',').find(|r| r.starts_with(&spec))?;
            let attr = *parse_set(link).get(3)?;
            for unit in units(file, name) {
                let Some(entry) = here.get(unit).filter(|e| !e.is_null()) else {
                    continue;
                };
                let key = scalar(entry.get(attr)?)?;
                let row = joined.entry(key.clone()).or_insert_with(|| {
                    order.push(key.clone());
                    Row {
                        related: Some(key.clone()),
                        entries: Vec::new(),
                    }
                });
                let id = (service.clone(), source.clone(), name.to_string());
                row.entries.retain(|(k, _)| *k != id);
                row.entries.push((id, entry));
            }
        }
    }
    let index = |k: &str| -> Option<u32> {
        (k == "0" || !k.starts_with('0'))
            .then(|| k.parse::<u32>().ok())
            .flatten()
            .filter(|n| *n < u32::MAX)
    };
    let (mut ints, rest): (Vec<String>, Vec<String>) =
        order.into_iter().partition(|k| index(k).is_some());
    ints.sort_by_key(|k| index(k));
    Some(
        ints.into_iter()
            .chain(rest)
            .filter_map(|k| joined.remove(&k))
            .collect(),
    )
}

/// A JSON scalar as the script reads it into a string.
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A token's raw value in one record: `None` when the record does not publish
/// it (the row is then dropped unless the token is optional).
pub(super) fn value<'a>(row: &Row<'a>, t: &Token<'_>) -> Option<&'a Value> {
    if t.related {
        return None;
    }
    let mut parts = t.path.split('/');
    let (dir, file) = (parts.next()?, parts.next()?);
    let attr = t
        .attr
        .as_deref()
        .unwrap_or(if t.string_of { "" } else { "price" });
    row.entry(dir, file, &t.key)?.get(attr)
}

/// The related value, for `{bulkStringOf!related}`.
pub(super) fn related_value(row: &Row<'_>) -> Option<String> {
    row.related.clone()
}

/// Sort the records as the script does, or `None` for an order this contract
/// has not read.
pub(super) fn sort(rows: &mut Vec<Row<'_>>, meta: &Sets<'_>) -> Option<()> {
    let order = match meta.sort_order {
        "asc" => Order::Asc,
        "desc" => Order::Desc,
        "instance-capacity" | "instanceCapacityAsc" => Order::Capacity,
        "instanceCapacityDesc" => Order::CapacityDesc,
        "instanceFamilyAsc" => Order::Family,
        // Named orders this contract has not read.
        "comparator"
        | "instanceType"
        | "sagemakerComponentType"
        | "sortStaticallyOrderedListVals"
        | "purchaseOption"
        | "regionAsc"
        | "locationsAsc"
        | "instanceCapacity"
        | "getInstanceCapacityWeight"
        | "getInstanceFamilyWeight" => return None,
        // Not a named order: the script compares ascending.
        _ => Order::Asc,
    };
    let property = if meta.sort_property.is_empty() {
        "related"
    } else {
        meta.sort_property
    };
    let path = parse_set(property);
    let key = |row: &Row<'_>| -> Option<String> {
        match path.as_slice() {
            ["related"] => row.related.clone(),
            [dir, file, set, attr] => row.entry(dir, file, set)?.get(*attr).and_then(scalar),
            // A path that ends on an object compares every record equal.
            [_, _, _] => Some("[object Object]".into()),
            _ => None,
        }
    };
    let keys: Vec<Option<String>> = rows.iter().map(key).collect();
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    merge_sort(&mut idx, &|a, b| match (&keys[*a], &keys[*b]) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => sign(order.compare(x, y)),
    });
    let mut taken: Vec<Option<Row<'_>>> = std::mem::take(rows).into_iter().map(Some).collect();
    rows.extend(idx.into_iter().filter_map(|i| taken[i].take()));
    Some(())
}

fn sign(v: f64) -> Ordering {
    if v < 0.0 {
        Ordering::Less
    } else if v > 0.0 {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

/// A stable merge sort that tolerates a comparator that is not a total order
/// (the script's is not, in corners), where the standard sort may panic.
fn merge_sort(v: &mut [usize], cmp: &dyn Fn(&usize, &usize) -> Ordering) {
    if v.len() < 2 {
        return;
    }
    let mid = v.len() / 2;
    merge_sort(&mut v[..mid], cmp);
    merge_sort(&mut v[mid..], cmp);
    let (left, right) = (v[..mid].to_vec(), v[mid..].to_vec());
    let (mut i, mut j, mut k) = (0, 0, 0);
    while i < left.len() && j < right.len() {
        if cmp(&right[j], &left[i]) == Ordering::Less {
            v[k] = right[j];
            j += 1;
        } else {
            v[k] = left[i];
            i += 1;
        }
        k += 1;
    }
    for x in left[i..].iter().chain(&right[j..]) {
        v[k] = *x;
        k += 1;
    }
}

#[derive(Clone, Copy)]
enum Order {
    Asc,
    Desc,
    Capacity,
    CapacityDesc,
    Family,
}

impl Order {
    fn compare(self, a: &str, b: &str) -> f64 {
        match self {
            Order::Asc => comparator(a, b),
            Order::Desc => comparator(b, a),
            Order::Capacity => capacity(a, b),
            Order::CapacityDesc => capacity(b, a),
            Order::Family => family_asc(&Js::Str(a.into()), &Js::Str(b.into())),
        }
    }
}

/// `Number(s)` for a string.
fn to_number(s: &str) -> f64 {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if t.is_empty() {
        return 0.0;
    }
    let (sign, body) = match t.as_bytes()[0] {
        b'-' => (-1.0, &t[1..]),
        b'+' => (1.0, &t[1..]),
        _ => (1.0, t),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = t.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).map_or(f64::NAN, |n| n as f64);
        }
    }
    let ok = !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        && body
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit() || b == b'.');
    if !ok {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

/// JavaScript's `<` on two strings: UTF-16 code units.
fn js_less(a: &str, b: &str) -> bool {
    a.encode_utf16().lt(b.encode_utf16())
}

/// The script's `comparator`: numerically when both are numbers, else as strings.
fn comparator(a: &str, b: &str) -> f64 {
    let (x, y) = (to_number(a), to_number(b));
    if !x.is_nan() && !y.is_nan() {
        return x - y;
    }
    if js_less(a, b) {
        -1.0
    } else if js_less(b, a) {
        1.0
    } else {
        0.0
    }
}

const SIZES: [&str; 8] = [
    "nano", "micro", "small", "medium", "large", "xlplus", "xlarge", "metal",
];
/// `/^a(\d+)(.*)$/i` and its siblings, in the script's order.
const FAMILIES: [&str; 17] = [
    "a", "t", "m", "c", "cc", "p", "g", "cg", "f", "x", "r", "z", "cr", "i", "h", "d", "hs",
];

/// A value the script holds as a string, or as the `undefined` a lookup gave.
#[derive(Clone, Debug)]
enum Js {
    Str(String),
    Undefined,
}

impl Js {
    /// The string a regular expression sees.
    fn text(&self) -> &str {
        match self {
            Js::Str(s) => s,
            Js::Undefined => "undefined",
        }
    }
}

/// `family regex k` on `s`: its generation digits and the rest.
fn family_match(s: &str, k: usize) -> Option<(&str, &str)> {
    let prefix = FAMILIES[k];
    if s.len() < prefix.len() || !s.is_char_boundary(prefix.len()) {
        return None;
    }
    if !s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = &s[prefix.len()..];
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    // `.` does not match a line terminator.
    (digits > 0 && !rest.contains(['\n', '\r', '\u{2028}', '\u{2029}']))
        .then(|| (&rest[..digits], &rest[digits..]))
}

fn family_index(s: &str) -> Option<usize> {
    (0..FAMILIES.len()).find(|k| family_match(s, *k).is_some())
}

/// The script's `instanceFamilyAsc`.
fn family_asc(a: &Js, b: &Js) -> f64 {
    let (ea, eb) = (a.text(), b.text());
    let (sa, sb) = (family_index(ea), family_index(eb));
    let idx = |i: Option<usize>| i.map_or(-1.0, |i| i as f64);
    // The generation as the script holds it: the digits as a string, or -1.
    let generation = |e: &str, k: Option<usize>| -> Result<String, ()> {
        match k {
            Some(k) => Ok(family_match(e, k)
                .map(|m| m.0.to_string())
                .unwrap_or_default()),
            None => {
                let start = e.find(|c: char| c.is_ascii_digit()).ok_or(())?;
                let len = e[start..].bytes().take_while(u8::is_ascii_digit).count();
                Ok(e[start..start + len].to_string())
            }
        }
    };
    let suffix = |e: &str, k: Option<usize>| -> String {
        match k {
            Some(k) => family_match(e, k)
                .map(|m| m.1.to_string())
                .unwrap_or_default(),
            None => {
                let tail = e.bytes().rev().take_while(u8::is_ascii_lowercase).count();
                e[e.len() - tail..].to_string()
            }
        }
    };
    if sa == sb {
        match (generation(ea, sa), generation(eb, sb)) {
            (Ok(u), Ok(f)) => {
                if u != f {
                    to_number(&f) - to_number(&u)
                } else if js_less(&suffix(ea, sa), &suffix(eb, sb)) {
                    -1.0
                } else {
                    1.0
                }
            }
            (Err(()), Err(())) => comparator(ea, eb),
            (u, f) => {
                let n = |g: Result<String, ()>| g.map_or(-1.0, |s| to_number(&s));
                n(f) - n(u)
            }
        }
    } else if sa.is_none() || sb.is_none() {
        idx(sb) - idx(sa)
    } else {
        idx(sa) - idx(sb)
    }
}

/// `parseInt(s, 10)`.
fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let len = body.bytes().take_while(u8::is_ascii_digit).count();
    let n: i64 = body[..len].parse().ok()?;
    Some(if neg { -n } else { n })
}

/// The script's size weight: `1000 × size index + count`, or -1.
fn size_weight(size: &Js) -> f64 {
    let Js::Str(s) = size else {
        return -1.0;
    };
    let n = parse_int(s);
    let name = match n {
        None => s.as_str(),
        Some(n) => {
            // `RegExp(n + "(\\w*)").exec(size)[1]`
            let needle = n.to_string();
            let Some(at) = s.find(&needle) else {
                return -1.0;
            };
            let rest = &s[at + needle.len()..];
            let len = rest
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            &rest[..len]
        }
    };
    let Some(i) = SIZES.iter().position(|z| *z == name) else {
        return -1.0;
    };
    let base = 1000.0 * i as f64;
    n.map_or(base, |n| n as f64 + base)
}

/// The script's `instance-capacity` order: family by family (newest
/// generation first), then by size within a family.
fn capacity(a: &str, b: &str) -> f64 {
    if !to_number(a).is_nan() || !to_number(b).is_nan() {
        return comparator(a, b);
    }
    let size_part = |e: &str| -> Js {
        e.split('.')
            .find(|p| SIZES.iter().any(|z| p.contains(z)))
            .map_or(Js::Undefined, |p| Js::Str(p.to_string()))
    };
    let family_part = |e: &str, size: &Js| -> Js {
        if let Some(p) = e.split('.').find(|p| family_index(p).is_some()) {
            return Js::Str(p.to_string());
        }
        match size {
            Js::Str(sz) => {
                let parts: Vec<&str> = e.split('.').collect();
                let at = parts.iter().position(|p| p == sz).unwrap_or(0);
                at.checked_sub(1)
                    .map_or(Js::Undefined, |i| Js::Str(parts[i].to_string()))
            }
            Js::Undefined => Js::Str(word_with_digits(e).unwrap_or(e).to_string()),
        }
    };
    let (da, db) = (size_part(a), size_part(b));
    let (ha, hb) = (family_part(a, &da), family_part(b, &db));
    let same = match (&ha, &hb) {
        (Js::Str(x), Js::Str(y)) => x == y,
        (Js::Undefined, Js::Undefined) => true,
        _ => false,
    };
    if same {
        let (s, l) = (size_weight(&da), size_weight(&db));
        if s == -1.0 || l == -1.0 {
            if s == -1.0 && l == -1.0 {
                comparator(a, b)
            } else {
                l - s
            }
        } else {
            s - l
        }
    } else {
        family_asc(&ha, &hb)
    }
}

/// `/\b[a-z]+\d+[a-z]*\b/` on `e`.
fn word_with_digits(e: &str) -> Option<&str> {
    let b = e.as_bytes();
    let word = |i: usize| {
        b.get(i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
    };
    for start in 0..b.len() {
        if !b[start].is_ascii_lowercase() || (start > 0 && word(start - 1)) {
            continue;
        }
        let mut i = start;
        while i < b.len() && b[i].is_ascii_lowercase() {
            i += 1;
        }
        // Backtrack over the letters so that digits follow.
        for letters_end in (start + 1..=i).rev() {
            let mut j = letters_end;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j == letters_end {
                continue;
            }
            let mut k = j;
            while k < b.len() && b[k].is_ascii_lowercase() {
                k += 1;
            }
            for end in (j..=k).rev() {
                if !word(end) {
                    return Some(&e[start..end]);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(order: Order, items: &[&str]) -> Vec<String> {
        let mut idx: Vec<usize> = (0..items.len()).collect();
        merge_sort(&mut idx, &|a, b| sign(order.compare(items[*a], items[*b])));
        idx.into_iter().map(|i| items[i].to_string()).collect()
    }

    /// The order the page's own script gave (run under Node on the bundle's
    /// sort module) for the same RDS instance types.
    #[test]
    fn instance_capacity_orders_newest_family_first_then_by_size() {
        let got = sorted(
            Order::Capacity,
            &[
                "db.m5.large",
                "db.m7g.2xlarge",
                "db.t3.micro",
                "db.m7g.large",
                "db.r6g.xlarge",
                "db.m5.24xlarge",
                "db.t4g.medium",
                "db.m5d.large",
                "db.x2g.16xlarge",
                "db.m6i.metal",
                "db.m7g.12xlarge",
            ],
        );
        assert_eq!(
            got,
            [
                "db.t4g.medium",
                "db.t3.micro",
                "db.m7g.large",
                "db.m7g.2xlarge",
                "db.m7g.12xlarge",
                "db.m6i.metal",
                "db.m5.large",
                "db.m5.24xlarge",
                "db.m5d.large",
                "db.x2g.16xlarge",
                "db.r6g.xlarge",
            ]
        );
    }

    #[test]
    fn instance_capacity_matches_the_script_across_services_and_odd_names() {
        let got = sorted(
            Order::Capacity,
            &[
                "cache.t4g.micro",
                "cache.m7g.xlarge",
                "cache.r7g.large",
                "cache.c7gn.large",
                "cache.m6g.large",
                "cache.t3.small",
                "cache.r6gd.xlarge",
                "ml.m5.large",
                "ml.g5.2xlarge",
                "ml.p4d.24xlarge",
                "ml.c5.xlarge",
                "ml.t3.medium",
                "foo",
                "8",
                "db.m6id.metal-48xl",
                "db.x2iedn.32xlarge",
            ],
        );
        assert_eq!(
            got,
            [
                "8",
                "cache.t4g.micro",
                "cache.t3.small",
                "ml.t3.medium",
                "cache.m7g.xlarge",
                "cache.m6g.large",
                "db.m6id.metal-48xl",
                "ml.m5.large",
                "cache.c7gn.large",
                "ml.c5.xlarge",
                "ml.p4d.24xlarge",
                "ml.g5.2xlarge",
                "db.x2iedn.32xlarge",
                "cache.r7g.large",
                "cache.r6gd.xlarge",
                "foo",
            ]
        );
    }

    #[test]
    fn asc_compares_numbers_as_numbers_and_the_rest_as_strings() {
        assert_eq!(sorted(Order::Asc, &["10", "9", "100"]), ["9", "10", "100"]);
        assert_eq!(sorted(Order::Asc, &["b", "a", "B"]), ["B", "a", "b"]);
        assert!(to_number("").abs() < f64::EPSILON, "Number('') is 0");
        assert!(to_number("12 GiB").is_nan());
        assert!(to_number("inf").is_nan());
        assert_eq!(to_number(" 1e3 "), 1000.0);
    }

    #[test]
    fn a_set_spec_splits_as_the_script_splits_it() {
        assert_eq!(
            parse_set("rds/rds-mysql-ondemand.[Database Instance MySQL].[Instance Type]"),
            [
                "rds",
                "rds-mysql-ondemand",
                "Database Instance MySQL",
                "Instance Type"
            ]
        );
        assert_eq!(
            parse_set("docdb/docdb.[Memory optimized OnDemand]"),
            ["docdb", "docdb", "Memory optimized OnDemand"]
        );
        assert_eq!(parse_set("related"), ["related"]);
    }

    #[test]
    fn related_rows_enumerate_integer_keys_first() {
        let file = serde_json::json!({
            "manifest": {"serviceId": "s", "source": "f"},
            "sets": {"A": ["u1", "u2", "u3"]},
            "regions": {"R": {
                "u1": {"k": "b", "price": "1"},
                "u2": {"k": "10", "price": "2"},
                "u3": {"k": "2", "price": "3"},
            }}
        });
        let sets = vec!["s/f.[A]".to_string()];
        let rows = related(&[&file], "R", &sets, "s/f.[A].[k]").unwrap();
        let keys: Vec<_> = rows.iter().map(|r| r.related.clone().unwrap()).collect();
        assert_eq!(keys, ["2", "10", "b"]);
    }
}
