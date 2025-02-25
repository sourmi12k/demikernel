// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use super::*;
use crate::{
    inetstack::test_helpers,
    runtime::timer::SharedTimer,
};
use ::anyhow::Result;
use ::std::time::Instant;

/// Tests that an entry of the ARP Cache gets evicted at the right time.
#[test]
fn evict_with_default_ttl() -> Result<()> {
    let now = Instant::now();
    let ttl = Duration::from_secs(1);
    let later = now + ttl;
    let mut clock = SharedTimer::new(now);

    // Insert an IPv4 address in the ARP Cache.
    let mut cache = ArpCache::new(clock.clone(), Some(ttl), None, false);
    cache.insert(test_helpers::ALICE_IPV4, test_helpers::ALICE_MAC);
    crate::ensure_eq!(cache.get(test_helpers::ALICE_IPV4), Some(&test_helpers::ALICE_MAC));

    // Advance the internal clock of the cache and clear it.
    clock.advance_clock(later);
    cache.clear();

    // The IPv4 address must be gone.
    crate::ensure_eq!(cache.get(test_helpers::ALICE_IPV4), None);

    Ok(())
}

/// Tests import on the ARP Cache.
#[test]
fn import() -> Result<()> {
    let now = Instant::now();
    let ttl = Duration::from_secs(1);
    let clock = SharedTimer::new(now);

    // Create an address resolution map.
    let mut map: HashMap<Ipv4Addr, MacAddress> = HashMap::new();
    map.insert(test_helpers::ALICE_IPV4, test_helpers::ALICE_MAC);

    // Create an ARP Cache and import address resolution map.
    let cache = ArpCache::new(clock, Some(ttl), Some(&map), false);

    // Check if address resolutions are in the ARP Cache.
    crate::ensure_eq!(cache.get(test_helpers::ALICE_IPV4), Some(&test_helpers::ALICE_MAC));

    Ok(())
}

/// Tests export on the ARP Cache.
#[test]
fn export() -> Result<()> {
    let now = Instant::now();
    let ttl = Duration::from_secs(1);
    let clock = SharedTimer::new(now);

    // Insert an IPv4 address in the ARP Cache.
    let mut cache = ArpCache::new(clock, Some(ttl), None, false);
    cache.insert(test_helpers::ALICE_IPV4, test_helpers::ALICE_MAC);
    crate::ensure_eq!(cache.get(test_helpers::ALICE_IPV4), Some(&test_helpers::ALICE_MAC));

    // Export address resolution map.
    let map: HashMap<Ipv4Addr, MacAddress> = cache.export();

    // Check if address resolutions are in the map that was exported.
    crate::ensure_eq!(
        map.get_key_value(&test_helpers::ALICE_IPV4),
        Some((&test_helpers::ALICE_IPV4, &test_helpers::ALICE_MAC))
    );

    Ok(())
}
