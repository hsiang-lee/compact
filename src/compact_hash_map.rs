extern crate primal;

use super::compact::Compact;
use super::compact_vec::CompactVec;
use super::simple_allocator_trait::{Allocator, DefaultHeap};
use std::collections::hash_map::DefaultHasher;
#[cfg(test)]
use std::collections::HashMap;
use std::hash::Hash;
use std::hash::Hasher;
use std::iter::Iterator;

use std;
use std::fmt::Write;

#[derive(Clone)]
struct Entry<K, V> {
    hash: u32,
    tombstoned: bool,
    inner: Option<(K, V)>,
}

struct QuadraticProbingIterator<'a, K: 'a, V: 'a, A: 'a + Allocator = DefaultHeap> {
    i: usize,
    number_used: usize,
    hash: u32,
    map: &'a OpenAddressingMap<K, V, A>,
}

struct QuadraticProbingMutIterator<'a, K: 'a, V: 'a, A: 'a + Allocator = DefaultHeap> {
    i: usize,
    number_used: usize,
    hash: u32,
    map: &'a mut OpenAddressingMap<K, V, A>,
}

/// A dynamically-sized open adressing quadratic probing hashmap
/// that can be stored in compact sequential storage and
/// automatically spills over into free heap storage using `Allocator`.
pub struct OpenAddressingMap<K, V, A: Allocator = DefaultHeap> {
    number_alive: u32,
    number_used: u32,
    entries: CompactVec<Entry<K, V>, A>,
}

impl<K: Eq, V: Clone> Entry<K, V> {
    fn make_used(&mut self, hash: u32, key: K, value: V) {
        self.hash = hash;
        self.inner = Some((key, value));
    }

    fn replace_value(&mut self, new_val: V) -> Option<V> {
        debug_assert!(self.used());
        match self.inner.as_mut() {
            None => None,
            Some(kv) => {
                let old = kv.1.clone();
                kv.1 = new_val;
                Some(old)
            }
        }
    }

    fn remove(&mut self) -> Option<V> {
        let old_val = self.value_option().cloned();
        self.inner = None;
        self.tombstoned = true;
        old_val
    }

    fn used(&self) -> bool {
        self.tombstoned || self.inner.is_some()
    }

    fn alive(&self) -> bool {
        self.inner.is_some()
    }

    fn free(&self) -> bool {
        self.inner.is_none() && (!self.tombstoned)
    }

    fn key(&self) -> &K {
        &self.inner.as_ref().unwrap().0
    }

    fn value(&self) -> &V {
        self.inner.as_ref().map(|kv| &kv.1).unwrap()
    }

    fn value_option(&self) -> Option<&V> {
        self.inner.as_ref().map(|kv| &kv.1)
    }

    fn mut_value(&mut self) -> &mut V {
        self.inner.as_mut().map(|kv| &mut kv.1).unwrap()
    }

    fn mut_value_option(&mut self) -> Option<&mut V> {
        self.inner.as_mut().map(|kv| &mut kv.1)
    }

    fn is_this(&self, key: &K) -> bool {
        self.inner.as_ref().map_or(false, |kv| &kv.0 == key)
    }

    fn into_tuple(self) -> (K, V) {
        debug_assert!(self.alive());
        let kv = self.inner.unwrap();
        (kv.0, kv.1)
    }
}

impl<K, V> std::fmt::Debug for Entry<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Entry {:?}, {:?}", self.hash, self.inner.is_some())
    }
}

impl<K, V> Default for Entry<K, V> {
    fn default() -> Self {
        Entry {
            hash: 0,
            tombstoned: false,
            inner: None,
        }
    }
}

impl<K: Copy, V: Compact> Compact for Entry<K, V> {
    fn is_still_compact(&self) -> bool {
        if std::mem::needs_drop::<V>() {
            if self.tombstoned {
                true
            } else {
                self.inner
                    .as_ref()
                    .map_or(true, |kv_tuple| kv_tuple.1.is_still_compact())
            }
        } else {
            true
        }
    }

    fn dynamic_size_bytes(&self) -> usize {
        if std::mem::needs_drop::<V>() {
            if self.tombstoned {
                0
            } else {
                self.inner
                    .as_ref()
                    .map_or(0, |kv_tuple| kv_tuple.1.dynamic_size_bytes())
            }
        } else {
            0
        }
    }

    unsafe fn compact(source: *mut Self, dest: *mut Self, new_dynamic_part: *mut u8) {
        (*dest).hash = (*source).hash;
        (*dest).tombstoned = (*source).tombstoned;

        if std::mem::needs_drop::<V>() {
            ::std::ptr::copy_nonoverlapping(&(*source).inner, &mut (*dest).inner, 1);
            if (*dest).inner.is_some() {
                Compact::compact(
                    &mut (*source).inner.as_mut().unwrap().1,
                    &mut (*dest).inner.as_mut().unwrap().1,
                    new_dynamic_part,
                )
            }
        } else {
            (*dest).inner = std::ptr::read(&(*source).inner);
        }
    }

    unsafe fn decompact(source: *const Self) -> Entry<K, V> {
        if (*source).inner.is_none() {
            Entry {
                hash: (*source).hash,
                tombstoned: (*source).tombstoned,
                inner: None,
            }
        } else if std::mem::needs_drop::<V>() {
            let insides = (*source).inner.as_ref().unwrap();
            Entry {
                hash: (*source).hash,
                tombstoned: (*source).tombstoned,
                inner: Some((insides.0, (Compact::decompact(&insides.1)))),
            }
        } else {
            Entry {
                hash: (*source).hash,
                tombstoned: (*source).tombstoned,
                inner: std::ptr::read(&(*source).inner),
            }
        }
    }
}

lazy_static! {
    static ref PRIME_SIEVE: primal::Sieve = primal::Sieve::new(1_000_000);
}

impl<'a, K: Copy, V: Compact, A: Allocator> QuadraticProbingIterator<'a, K, V, A> {
    fn for_map(
        map: &'a OpenAddressingMap<K, V, A>,
        hash: u32,
    ) -> QuadraticProbingIterator<'a, K, V, A> {
        QuadraticProbingIterator {
            i: 0,
            number_used: map.entries.capacity(),
            hash,
            map,
        }
    }
}

impl<'a, K: Copy, V: Compact, A: Allocator> QuadraticProbingMutIterator<'a, K, V, A> {
    fn for_map(
        map: &'a mut OpenAddressingMap<K, V, A>,
        hash: u32,
    ) -> QuadraticProbingMutIterator<'a, K, V, A> {
        QuadraticProbingMutIterator {
            i: 0,
            number_used: map.entries.capacity(),
            hash,
            map,
        }
    }
}

impl<'a, K, V, A: Allocator> Iterator for QuadraticProbingIterator<'a, K, V, A> {
    type Item = &'a Entry<K, V>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.i >= self.number_used {
            return None;
        }
        let index = (self.hash as usize + self.i * self.i) % self.number_used;
        self.i += 1;
        Some(&self.map.entries[index])
    }
}

impl<'a, K, V, A: Allocator> Iterator for QuadraticProbingMutIterator<'a, K, V, A> {
    type Item = &'a mut Entry<K, V>;
    fn next(&mut self) -> Option<&'a mut Entry<K, V>> {
        if self.i >= self.number_used {
            return None;
        }
        let index = (self.hash as usize + self.i * self.i) % self.number_used;
        self.i += 1;
        Some(unsafe { &mut *(&mut self.map.entries[index] as *mut Entry<K, V>) })
    }
}

impl<K: Copy + Eq + Hash, V: Compact, A: Allocator> OpenAddressingMap<K, V, A> {
    /// constructor
    pub fn new() -> Self {
        Self::with_capacity(4)
    }
    /// constructor
    pub fn with_capacity(l: usize) -> Self {
        OpenAddressingMap {
            entries: vec![Entry::default(); Self::find_next_prime(l)].into(),
            number_alive: 0,
            number_used: 0,
        }
    }

    /// Amount of entries in the dictionary
    pub fn len(&self) -> usize {
        self.number_alive as usize
    }

    /// Amount of used entries in the dictionary
    #[cfg(test)]
    pub fn len_used(&self) -> usize {
        self.number_used as usize
    }

    /// Capacity of the dictionary
    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    /// Is the dictionary empty?
    pub fn is_empty(&self) -> bool {
        self.number_alive == 0
    }

    /// Look up the value for key `query`, if it exists
    pub fn get(&self, query: K) -> Option<&V> {
        self.find_used(query).and_then(|e| e.value_option())
    }

    /// get mutable
    pub fn get_mut(&mut self, query: K) -> Option<&mut V> {
        self.find_used_mut(query).and_then(|e| e.mut_value_option())
    }

    /// Does the dictionary contain a value for `query`?
    pub fn contains_key(&self, query: K) -> bool {
        self.get(query).map_or(false, |_| true)
    }

    /// Insert new value at key `query` and return the previous value at that key, if any existed
    pub fn insert(&mut self, query: K, value: V) -> Option<V> {
        self.insert_inner_growing(query, value)
    }

    /// Remove value at key `query` and return it, if it existed
    pub fn remove(&mut self, query: K) -> Option<V> {
        self.remove_inner(query)
    }

    /// Iterator over all keys in the dictionary
    pub fn keys<'a>(&'a self) -> impl Iterator<Item = &'a K> + 'a {
        self.entries.iter().filter(|e| e.alive()).map(|e| e.key())
    }

    /// Iterator over all values in the dictionary
    pub fn values<'a>(&'a self) -> impl Iterator<Item = &'a V> + 'a {
        self.entries.iter().filter(|e| e.alive()).map(|e| e.value())
    }

    /// Iterator over mutable references to all values in the dictionary
    pub fn values_mut<'a>(&'a mut self) -> impl Iterator<Item = &'a mut V> + 'a {
        self.entries
            .iter_mut()
            .filter(|e| e.alive())
            .map(|e| e.mut_value())
    }

    /// Iterator over all key-value pairs in the dictionary
    pub fn pairs<'a>(&'a self) -> impl Iterator<Item = (&'a K, &'a V)> + 'a {
        self.entries
            .iter()
            .filter(|e| e.alive())
            .map(|e| (e.key(), e.value()))
    }

    /// Iterator over all key-value pairs in the dictionary,
    /// with the value as a mutable reference
    pub fn pairs_mut<'a>(&'a mut self) -> impl Iterator<Item = (K, &'a mut V)> + 'a
    where
        K: Copy,
    {
        self.entries
            .iter_mut()
            .filter(|e| e.alive())
            .map(|e| (*e.key(), e.mut_value()))
    }

    fn hash(key: K) -> u32 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish() as u32
    }

    fn insert_inner_growing(&mut self, query: K, value: V) -> Option<V> {
        self.ensure_capacity();
        self.insert_inner(query, value)
    }

    fn insert_inner(&mut self, query: K, value: V) -> Option<V> {
        let res = self.insert_inner_inner(query, value);
        if res.is_none() {
            self.number_alive += 1;
            self.number_used += 1;
        }
        res
    }

    fn insert_inner_inner(&mut self, query: K, value: V) -> Option<V> {
        let hash = Self::hash(query);
        for entry in self.quadratic_iterator_mut(hash) {
            if entry.free() {
                entry.make_used(hash, query, value);
                return None;
            } else if entry.is_this(&query) {
                return entry.replace_value(value);
            }
        }
        panic!("should have place")
    }

    fn remove_inner(&mut self, query: K) -> Option<V> {
        // remove inner does not alter the size because of tombstones
        let old = self.remove_inner_inner(query);
        if old.is_some() {
            self.number_alive -= 1;
        }
        old
    }

    fn remove_inner_inner(&mut self, query: K) -> Option<V> {
        let hash = Self::hash(query);
        for entry in self.quadratic_iterator_mut(hash) {
            if entry.is_this(&query) {
                return entry.remove();
            }
        }
        None
    }

    fn ensure_capacity(&mut self) {
        if self.number_used as usize > self.entries.capacity() / 2 {
            let mut new_capacity = self.entries.capacity() * 2;

            // if there are lots of dead entries we do not need to double
            // we are going to just garbage collect them
            let number_dead = self.entries.capacity() - self.number_alive as usize;
            if number_dead > self.entries.capacity() / 2 {
                new_capacity = self.entries.capacity();
            }

            let mut new_hash_map = Self::with_capacity(new_capacity);

            for entry in self.entries.drain() {
                if entry.alive() {
                    let tuple = entry.into_tuple();
                    new_hash_map.insert(tuple.0, tuple.1);
                }
            }

            *self = new_hash_map;
        }
    }

    fn find_used(&self, query: K) -> Option<&Entry<K, V>> {
        for entry in self.quadratic_iterator(query) {
            if entry.is_this(&query) {
                return Some(entry);
            }
        }
        None
    }

    fn find_used_mut(&mut self, query: K) -> Option<&mut Entry<K, V>> {
        let h = Self::hash(query);
        for entry in self.quadratic_iterator_mut(h) {
            if entry.is_this(&query) {
                return Some(entry);
            }
        }
        None
    }

    fn quadratic_iterator(&self, query: K) -> QuadraticProbingIterator<'_, K, V, A> {
        QuadraticProbingIterator::for_map(self, Self::hash(query))
    }

    fn quadratic_iterator_mut(&mut self, hash: u32) -> QuadraticProbingMutIterator<'_, K, V, A> {
        QuadraticProbingMutIterator::for_map(self, hash)
    }

    fn find_next_prime(n: usize) -> usize {
        PRIME_SIEVE.primes_from(n).find(|&i| i >= n).unwrap()
    }

    fn display(&self) -> String {
        let mut res = String::new();
        writeln!(&mut res, "size: {:?}", self.number_alive).unwrap();
        let mut size_left: isize = self.number_alive as isize;
        for entry in self.entries.iter() {
            if entry.used() {
                size_left -= 1;
            }
            writeln!(&mut res, "  {:?} {:?}", entry.used(), entry.hash).unwrap();
        }
        writeln!(&mut res, "size_left : {:?}", size_left).unwrap();
        res
    }
}

impl<K: Copy + Eq + Hash, V: Compact, A: Allocator> Compact for OpenAddressingMap<K, V, A> {
    fn is_still_compact(&self) -> bool {
        self.entries.is_still_compact()
    }

    fn dynamic_size_bytes(&self) -> usize {
        self.entries.dynamic_size_bytes()
    }

    unsafe fn compact(source: *mut Self, dest: *mut Self, new_dynamic_part: *mut u8) {
        (*dest).number_alive = (*source).number_alive;
        (*dest).number_used = (*source).number_used;
        Compact::compact(
            &mut (*source).entries,
            &mut (*dest).entries,
            new_dynamic_part,
        );
    }

    unsafe fn decompact(source: *const Self) -> OpenAddressingMap<K, V, A> {
        OpenAddressingMap {
            entries: Compact::decompact(&(*source).entries),
            number_alive: (*source).number_alive,
            number_used: (*source).number_used,
        }
    }
}

impl<K: Copy, V: Compact + Clone, A: Allocator> Clone for OpenAddressingMap<K, V, A> {
    fn clone(&self) -> Self {
        OpenAddressingMap {
            entries: self.entries.clone(),
            number_alive: self.number_alive,
            number_used: self.number_used,
        }
    }
}

impl<K: Copy + Eq + Hash, V: Compact, A: Allocator> Default for OpenAddressingMap<K, V, A> {
    fn default() -> Self {
        OpenAddressingMap::with_capacity(5)
    }
}

impl<K: Copy + Eq + Hash, V: Compact + Clone, A: Allocator> ::std::iter::FromIterator<(K, V)>
    for OpenAddressingMap<K, V, A>
{
    /// Construct a compact dictionary from an interator over key-value pairs
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter_to_be: T) -> Self {
        let iter = iter_to_be.into_iter();
        let mut map = Self::with_capacity(iter.size_hint().0);
        for (key, value) in iter {
            map.insert(key, value);
        }
        map
    }
}

impl<
        K: Copy + Eq + Hash + ::std::fmt::Debug,
        V: Compact + Clone + ::std::fmt::Debug,
        A: Allocator,
    > ::std::fmt::Debug for OpenAddressingMap<K, V, A>
{
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        f.debug_map().entries(self.pairs()).finish()
    }
}

impl<K: Hash + Eq + Copy, I: Compact, A1: Allocator, A2: Allocator>
    OpenAddressingMap<K, CompactVec<I, A1>, A2>
{
    /// Push a value onto the `CompactVec` at the key `query`
    pub fn push_at(&mut self, query: K, item: I) {
        if self.push_at_inner(query, item) {
            self.number_alive += 1;
            self.number_used += 1;
        }
    }

    /// return true if new value pushed
    fn push_at_inner(&mut self, query: K, item: I) -> bool {
        self.ensure_capacity();
        let hash = Self::hash(query);
        for entry in self.quadratic_iterator_mut(hash) {
            if entry.is_this(&query) {
                entry.mut_value().push(item);
                return false;
            } else if !entry.used() {
                let mut val = CompactVec::new();
                val.push(item);
                entry.make_used(hash, query, val);
                return true;
            }
        }
        println!("{:?}", self.display());
        panic!("should always have place");
    }

    /// Iterator over the `CompactVec` at the key `query`
    pub fn get_iter<'a>(&'a self, query: K) -> impl Iterator<Item = &'a I> + 'a {
        self.get(query)
            .into_iter()
            .flat_map(|vec_in_option| vec_in_option.iter())
    }

    /// Remove the `CompactVec` at the key `query` and iterate over its elements (if it existed)
    pub fn remove_iter<'a>(&'a mut self, query: K) -> impl Iterator<Item = I> + 'a {
        self.remove(query)
            .into_iter()
            .flat_map(|vec_in_option| vec_in_option.into_iter())
    }
}

impl<T: Hash> Hash for CompactVec<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for elem in self {
            elem.hash(state);
        }
    }
}

#[cfg(feature = "serde-serialization")]
use serde::ser::SerializeMap;
#[cfg(feature = "serde-serialization")]
use std::marker::PhantomData;

#[cfg(feature = "serde-serialization")]
impl<K, V, A> ::serde::Serialize for OpenAddressingMap<K, V, A>
where
    K: Copy + Eq + Hash + ::serde::Serialize,
    V: Compact + ::serde::Serialize,
    A: Allocator,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ::serde::Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.len()))?;
        for (k, v) in self.pairs() {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

#[cfg(feature = "serde-serialization")]
struct OpenAddressingMapVisitor<K, V, A: Allocator> {
    marker: PhantomData<fn() -> OpenAddressingMap<K, V, A>>,
}

#[cfg(feature = "serde-serialization")]
impl<K, V, A: Allocator> OpenAddressingMapVisitor<K, V, A> {
    fn new() -> Self {
        OpenAddressingMapVisitor {
            marker: PhantomData,
        }
    }
}

#[cfg(feature = "serde-serialization")]
impl<'de, K, V, A> ::serde::de::Visitor<'de> for OpenAddressingMapVisitor<K, V, A>
where
    K: Copy + Eq + Hash + ::serde::de::Deserialize<'de>,
    V: Compact + ::serde::de::Deserialize<'de>,
    A: Allocator,
{
    type Value = OpenAddressingMap<K, V, A>;

    fn expecting(&self, formatter: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        formatter.write_str("A Compact Hash Map")
    }

    fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
    where
        M: ::serde::de::MapAccess<'de>,
    {
        let mut map = OpenAddressingMap::with_capacity(access.size_hint().unwrap_or(0));

        while let Some((key, value)) = access.next_entry()? {
            map.insert(key, value);
        }

        Ok(map)
    }
}

#[cfg(feature = "serde-serialization")]
impl<'de, K, V, A> ::serde::de::Deserialize<'de> for OpenAddressingMap<K, V, A>
where
    K: Copy + Eq + Hash + ::serde::de::Deserialize<'de>,
    V: Compact + ::serde::de::Deserialize<'de>,
    A: Allocator,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: ::serde::de::Deserializer<'de>,
    {
        deserializer.deserialize_map(OpenAddressingMapVisitor::new())
    }
}

#[cfg(test)]
fn elem(n: usize) -> usize {
    (n * n) as usize
}

#[test]
fn very_basic1() {
    let mut map: OpenAddressingMap<u32, u32> = OpenAddressingMap::with_capacity(2);
    map.insert(0, 54);
    assert!(*map.get(0).unwrap() == 54);
    map.insert(1, 48);
    assert!(*map.get(1).unwrap() == 48);
}

#[test]
fn very_basic2() {
    let mut map: OpenAddressingMap<u32, u32> = OpenAddressingMap::with_capacity(3);
    map.insert(0, 54);
    map.insert(1, 48);
    assert!(*map.get(0).unwrap() == 54);
    assert!(*map.get(1).unwrap() == 48);
}

#[test]
fn basic() {
    let n: usize = 10000;
    let mut map: OpenAddressingMap<usize, usize> = OpenAddressingMap::with_capacity(n);
    assert!(map.is_empty() == true);
    for i in 0..n {
        let e = elem(i);
        map.insert(i, e);
    }
    assert!(map.is_empty() == false);
    for i in 0..n {
        let test = map.get(i).unwrap();
        let exp = elem(i);
        assert!(*test == exp, " failed exp {:?}  was {:?}", exp, test);
    }
    assert!(map.len() == n as usize);
    assert!(*map.get(n - 1).unwrap() == elem(n - 1));
    assert!(*map.get(n - 100).unwrap() == elem(n - 100));
    assert!(map.contains_key(n - 300) == true);
    assert!(map.contains_key(n + 1) == false);
    assert!(map.remove(500) == Some(elem(500)));
    assert!(map.get(500).is_none());
}

#[test]
fn iter() {
    let mut map: OpenAddressingMap<usize, usize> = OpenAddressingMap::with_capacity(200);
    let n = 10;
    assert!(map.is_empty() == true);
    for n in 0..n {
        map.insert(n, n * n);
    }
    for k in map.keys() {
        println!(" k {:?}", k);
    }
    for n in 0..n {
        let mut keys = map.keys();
        assert!(
            keys.find(|&i| {
                println!("find {:?} {:?}", i, n);
                *i == n
            })
            .is_some(),
            "fail n {:?} ",
            n
        );
    }
    for n in 0..n {
        let mut values = map.values();
        assert!(values.find(|i| **i == elem(n)).is_some());
    }
}

#[test]
fn values_mut() {
    let mut map: OpenAddressingMap<usize, usize> = OpenAddressingMap::new();
    assert!(map.is_empty() == true);
    for n in 0..100 {
        map.insert(n, n * n);
    }
    {
        let mut values_mut = map.values_mut();
        for i in &mut values_mut {
            *i = *i + 1;
        }
    }
    for i in 0..100 {
        assert!(*map.get(i).unwrap() == i * i + 1);
    }
}

#[test]
fn pairs() {
    let mut map: OpenAddressingMap<usize, usize> = OpenAddressingMap::new();
    assert!(map.is_empty() == true);
    for n in 0..100 {
        map.insert(n, n * n);
    }
    for (key, value) in map.pairs() {
        assert!(elem(*key) == *value);
    }
}

#[test]
fn push_at() {
    let mut map: OpenAddressingMap<usize, CompactVec<usize>> = OpenAddressingMap::new();
    assert!(map.is_empty() == true);
    for n in 0..10000 {
        map.push_at(n, elem(n));
        map.push_at(n, elem(n) + 1);
    }

    for n in 0..10000 {
        println!("n {:?}", n);
        let mut iter = map.get_iter(n);
        assert!(iter.find(|&i| *i == elem(n)).is_some());
        let mut iter2 = map.get_iter(n);
        assert!(iter2.find(|&i| *i == elem(n) + 1).is_some());
    }
}

#[test]
fn remove_iter() {
    let mut map: OpenAddressingMap<usize, CompactVec<usize>> = OpenAddressingMap::new();
    assert!(map.is_empty() == true);
    for n in 0..1000 {
        map.push_at(n, elem(n));
        map.push_at(n, elem(n) + 1);
    }
    let target = 500;
    let mut iter = map.remove_iter(target);
    assert!(iter.find(|i| *i == elem(target)).is_some());
    assert!(iter.find(|i| *i == elem(target) + 1).is_some());
}

#[test]
fn ensure_capacity_works() {
    let mut map: OpenAddressingMap<usize, CompactVec<usize>> = OpenAddressingMap::new();
    assert!(map.is_empty() == true);
    for n in 0..100 {
        map.push_at(n, elem(n));
        map.push_at(n, elem(n) + 1);
    }
    assert!(map.is_empty() == false);
}

#[test]
fn insert_after_remove_works_same_hash() {
    // get 2 elems with the same hash
    let mut hash_to_usize: HashMap<u32, usize> = HashMap::new();
    let mut bad_pair_opt = None;
    for i in 0..<usize>::max_value() {
        if i % 10000 == 0 {
            println!("i {}", i);
        }
        let hash = OpenAddressingMap::<usize, usize>::hash(i);
        if hash_to_usize.contains_key(&hash) {
            let p: usize = *hash_to_usize.get(&hash).unwrap();
            bad_pair_opt = Some((i, p));
            break;
        }
        hash_to_usize.insert(hash, i);
    }

    type NestedType = OpenAddressingMap<usize, usize>;
    let mut map: NestedType = OpenAddressingMap::new();

    let bad_pair = bad_pair_opt.unwrap();
    println!("bad pair {:?}", bad_pair);
    map.insert(bad_pair.0, 1);
    println!("map {}", map.display());
    map.insert(bad_pair.1, 2);
    println!("map {}", map.display());
    map.remove(bad_pair.0);
    println!("map {}", map.display());
    map.insert(bad_pair.1, 3);
    println!("map {}", map.display());

    let mut n1 = 0;
    for (key, _) in map.pairs() {
        if *key == bad_pair.1 {
            n1 += 1;
        }
    }
    assert!(n1 == 1);
}

#[test]
fn compact_notcopy() {
    type NestedType = OpenAddressingMap<usize, CompactVec<usize>>;

    let mut map: NestedType = OpenAddressingMap::new();
    let assert_fun = |map: &NestedType, t: usize| {
        assert!(map
            .get(t)
            .unwrap()
            .into_iter()
            .find(|i| **i == elem(t))
            .is_some())
    };

    for n in 0..1000 {
        map.push_at(n, elem(n));
        map.push_at(n, elem(n) + 1);
    }
    assert_fun(&map, 500);
    let bytes = map.total_size_bytes();
    let storage = DefaultHeap::allocate(bytes);
    unsafe {
        Compact::compact_behind(&mut map, storage as *mut NestedType);
        ::std::mem::forget(map);
        assert_fun(&(*(storage as *mut NestedType)), 449);
        let decompacted = Compact::decompact(storage as *mut NestedType);
        assert_fun(&decompacted, 449);
        DefaultHeap::deallocate(storage, bytes);
    }
}

#[test]
fn compact_copy() {
    type NestedType = OpenAddressingMap<usize, usize>;

    let mut map: NestedType = OpenAddressingMap::new();
    let assert_fun = |map: &NestedType, t: usize| assert!(map.get(t).is_some());

    for n in 0..1000 {
        map.insert(n, elem(n));
    }
    assert_fun(&map, 500);
    let bytes = map.total_size_bytes();
    let storage = DefaultHeap::allocate(bytes);
    unsafe {
        Compact::compact_behind(&mut map, storage as *mut NestedType);
        ::std::mem::forget(map);
        assert_fun(&(*(storage as *mut NestedType)), 449);
        let decompacted = Compact::decompact(storage as *mut NestedType);
        assert_fun(&decompacted, 449);
        DefaultHeap::deallocate(storage, bytes);
    }
}

#[test]
fn map_len_is_the_amount_of_inserted_and_not_removed_items() {
    type Map = OpenAddressingMap<usize, usize>;
    let mut map: Map = OpenAddressingMap::new();
    for n in 0..1000 {
        map.insert(n, elem(n));
    }
    for n in 0..10 {
        map.remove(n);
    }
    assert_eq!(990, map.len());
}

#[test]
fn when_there_are_lots_of_dead_tombstoned_entries_capacity_is_not_doubled() {
    type Map = OpenAddressingMap<usize, usize>;
    let mut map: Map = OpenAddressingMap::new();
    for n in 0..1000 {
        map.insert(n, elem(n));
    }
    for n in 0..600 {
        map.remove(n);
    }
    println!("self {}", map.capacity());
    assert_eq!(400, map.len());
    assert_eq!(1000, map.len_used());
    assert_eq!(3203, map.capacity());
    for n in 0..1000 {
        map.insert(10000 + n, elem(n));
    }
    assert_eq!(1400, map.len());
    assert_eq!(3203, map.capacity());
}

#[test]
fn when_there_are_lots_of_few_tombstoned_entries_capacity_is_doubled() {
    type Map = OpenAddressingMap<usize, usize>;
    let mut map: Map = OpenAddressingMap::new();
    for n in 0..1000 {
        map.insert(n, elem(n));
    }
    for n in 0..60 {
        map.remove(n);
    }
    println!("self {}", map.capacity());
    assert_eq!(940, map.len());
    assert_eq!(1000, map.len_used());
    assert_eq!(3203, map.capacity());
    for n in 0..1000 {
        map.insert(10000 + n, elem(n));
    }
    assert_eq!(1940, map.len());
    assert_eq!(6421, map.capacity());
}
