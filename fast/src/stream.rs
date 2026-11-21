//! Raw time keys omit price and category unless the event is a purchase.

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct EventKey {
    pub person: u64,
    pub ts: i64,
}

#[derive(Clone, Copy)]
pub(super) struct Purchase {
    pub key: EventKey,
    pub price: u32,
    pub cat: u32,
}

#[derive(Default)]
pub(super) struct Payload {
    pub times: Vec<EventKey>,
    pub purchases: Vec<Purchase>,
}

impl Payload {
    #[inline]
    pub fn push(&mut self, key: EventKey, price: u32, cat: u32) -> usize {
        if price == 0 {
            let i = self.times.len();
            self.times.push(key);
            i * 2
        } else {
            let i = self.purchases.len();
            self.purchases.push(Purchase { key, price, cat });
            i * 2 + 1
        }
    }
}
