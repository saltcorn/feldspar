//! [`TopicIndex`]: who is subscribed to which topic of one stream (TODO.md
//! "Live updates" §5, "Fan-out is indexed by topic").
//!
//! The fan-out for a running stream holds one of these and routes each element
//! to the subscriptions **on its topic** and nobody else. Sending every element
//! to every socket and filtering there would cost elements × sockets, and a
//! per-user `job_status` stream with ten thousand users is exactly the case
//! where that is ten thousand times too much. A `Single` stream has one topic,
//! `None`, and so one bucket.
//!
//! Generic over the key and the value so it can be tested as the plain data
//! structure it is.

use std::collections::HashMap;
use std::hash::Hash;

/// Subscriptions bucketed by topic.
#[derive(Debug)]
pub struct TopicIndex<K, V> {
    by_topic: HashMap<Option<String>, HashMap<K, V>>,
    topic_of: HashMap<K, Option<String>>,
}

impl<K, V> Default for TopicIndex<K, V> {
    fn default() -> Self {
        TopicIndex {
            by_topic: HashMap::new(),
            topic_of: HashMap::new(),
        }
    }
}

impl<K: Clone + Eq + Hash, V> TopicIndex<K, V> {
    /// An empty index.
    pub fn new() -> TopicIndex<K, V> {
        TopicIndex::default()
    }

    /// Put `key` on `topic`, moving it there if it was on another one.
    pub fn insert(&mut self, topic: Option<String>, key: K, value: V) {
        self.remove(&key);
        self.topic_of.insert(key.clone(), topic.clone());
        self.by_topic.entry(topic).or_default().insert(key, value);
    }

    /// Take `key` off whichever topic it is on. An emptied topic is dropped,
    /// so an index that once routed to a million users does not keep a
    /// million empty buckets.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let topic = self.topic_of.remove(key)?;
        let bucket = self.by_topic.get_mut(&topic)?;
        let value = bucket.remove(key);
        if bucket.is_empty() {
            self.by_topic.remove(&topic);
        }
        value
    }

    /// The subscriptions on `topic`.
    pub fn on(&self, topic: Option<&str>) -> impl Iterator<Item = (&K, &V)> {
        self.by_topic
            .get(&topic.map(str::to_owned))
            .into_iter()
            .flat_map(HashMap::iter)
    }

    /// Every subscription, on every topic — what a lag of the whole stream is
    /// reported to.
    pub fn all(&self) -> impl Iterator<Item = (&K, &V)> {
        self.by_topic.values().flat_map(HashMap::iter)
    }

    /// The topic `key` is on, if it is in the index.
    pub fn topic_of(&self, key: &K) -> Option<Option<&str>> {
        self.topic_of.get(key).map(Option::as_deref)
    }

    /// How many subscriptions there are, on every topic.
    pub fn len(&self) -> usize {
        self.topic_of.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.topic_of.is_empty()
    }

    /// How many topics have at least one subscription.
    pub fn topics(&self) -> usize {
        self.by_topic.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys<'a>(it: impl Iterator<Item = (&'a u32, &'a &'static str)>) -> Vec<u32> {
        let mut keys: Vec<u32> = it.map(|(k, _)| *k).collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn an_element_reaches_its_topic_and_nobody_else() {
        let mut index = TopicIndex::new();
        index.insert(Some("alice".to_owned()), 1, "a1");
        index.insert(Some("alice".to_owned()), 2, "a2");
        index.insert(Some("bob".to_owned()), 3, "b");
        assert_eq!(keys(index.on(Some("alice"))), vec![1, 2]);
        assert_eq!(keys(index.on(Some("bob"))), vec![3]);
        assert_eq!(keys(index.on(Some("carol"))), Vec::<u32>::new());
        assert_eq!(keys(index.on(None)), Vec::<u32>::new());
        assert_eq!(keys(index.all()), vec![1, 2, 3]);
        assert_eq!(index.len(), 3);
        assert_eq!(index.topics(), 2);
    }

    #[test]
    fn a_single_stream_is_one_bucket() {
        let mut index = TopicIndex::new();
        index.insert(None, 1, "x");
        index.insert(None, 2, "y");
        assert_eq!(keys(index.on(None)), vec![1, 2]);
        assert_eq!(index.topics(), 1);
    }

    #[test]
    fn removing_the_last_subscription_drops_the_topic() {
        let mut index = TopicIndex::new();
        index.insert(Some("7".to_owned()), 1, "x");
        assert_eq!(index.remove(&1), Some("x"));
        assert_eq!(index.remove(&1), None, "a second remove finds nothing");
        assert!(index.is_empty());
        assert_eq!(index.topics(), 0);
    }

    #[test]
    fn re_inserting_a_key_moves_it() {
        let mut index = TopicIndex::new();
        index.insert(Some("7".to_owned()), 1, "x");
        index.insert(Some("8".to_owned()), 1, "y");
        assert_eq!(keys(index.on(Some("7"))), Vec::<u32>::new());
        assert_eq!(keys(index.on(Some("8"))), vec![1]);
        assert_eq!(index.topic_of(&1), Some(Some("8")));
        assert_eq!(index.len(), 1);
    }
}
