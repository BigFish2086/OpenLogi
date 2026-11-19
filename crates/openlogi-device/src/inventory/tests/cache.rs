//! The probe cache: what it keys, when an entry ages out, and when a cached answer is reused.
use super::*;
use crate::channel::scripted::{feature_error, ping_reply, register_error};

#[test]
fn direct_codename_prefers_hidpp_marketing_name_over_generic_os_name() {
    assert_eq!(
        preferred_direct_codename(Some("Wireless Mouse MX Master 2S"), "Mouse"),
        "Wireless Mouse MX Master 2S"
    );
    assert_eq!(preferred_direct_codename(None, "Mouse"), "Mouse");
}

#[test]
fn cache_dirty_tracks_only_persistable_keys() {
    // A system whose devices never persist (direct-only, or Unifying) must not
    // rewrite probe-cache.json on every refresh pass: the file's content
    // wouldn't change.
    let mut e = Enumerator::with_backend(ScriptedBackend::new(Vec::new()));
    let unifying = CacheKey::UnifyingSlot {
        receiver_uid: "DA2699E1".into(),
        slot: 1,
    };
    e.apply_outcomes(vec![CacheOutcome::Fresh(unifying.clone(), cache_entry())]);
    assert!(
        !e.cache_dirty,
        "non-persistable fresh probe dirtied the cache"
    );

    // Its eviction is equally invisible to the persisted file.
    let nobody = HashSet::new();
    for _ in 0..=CACHE_MISS_GRACE {
        e.evict_unseen(&nobody, &nobody);
    }
    assert!(!e.cache.contains_key(&unifying), "entry should be evicted");
    assert!(!e.cache_dirty, "non-persistable eviction dirtied the cache");

    // A Bolt probe is what the file stores — that one dirties it.
    let bolt = CacheKey::Bolt {
        unit_id: [1, 2, 3, 4],
    };
    e.apply_outcomes(vec![CacheOutcome::Fresh(bolt, cache_entry())]);
    assert!(
        e.cache_dirty,
        "persistable fresh probe must dirty the cache"
    );
}

fn direct_node(serial: Option<&str>) -> NodeInfo {
    NodeInfo {
        id: NodeId::from("/dev/hidraw3".to_string()),
        vendor_id: 0x046d,
        product_id: 0xb378,
        usage_page: 0xff43,
        usage_id: 0x0202,
        name: "MX KEYS S".to_string(),
        manufacturer: None,
        serial_number: serial.map(str::to_string),
    }
}

#[test]
fn a_direct_device_is_keyed_on_its_serial_when_the_backend_reports_one() {
    // A Bluetooth device's serial is its address, which is the same after it
    // has been away on another computer. The node it arrives under is not.
    assert_eq!(
        direct_cache_key(&direct_node(Some("D9:D2:69:B2:4B:96"))),
        CacheKey::DirectSerial("d9:d2:69:b2:4b:96".to_string()),
    );
}

#[test]
fn a_direct_device_without_a_serial_falls_back_to_its_node() {
    // Weaker, but still better than enumeration order, and it is all there is.
    assert!(matches!(
        direct_cache_key(&direct_node(None)),
        CacheKey::Direct(_)
    ));
    assert!(matches!(
        direct_cache_key(&direct_node(Some(""))),
        CacheKey::Direct(_)
    ));
}

#[test]
fn a_slot_keyed_entry_survives_the_grace_then_evicts() {
    let mut e = Enumerator::with_backend(ScriptedBackend::new(Vec::new()));
    // A slot can belong to a different device the moment this one leaves, so
    // what it memoizes stops being true once the device is gone for good.
    let key = CacheKey::UnifyingSlot {
        receiver_uid: "DA2699E1".to_string(),
        slot: 1,
    };
    e.cache.insert(key.clone(), cache_entry());
    let nobody = HashSet::new();
    for _ in 0..CACHE_MISS_GRACE {
        e.evict_unseen(&nobody, &nobody);
        assert!(
            e.cache.contains_key(&key),
            "evicted inside the grace window"
        );
    }

    e.evict_unseen(&nobody, &nobody);

    assert!(
        !e.cache.contains_key(&key),
        "should evict past the grace window"
    );
}

#[test]
fn a_serial_keyed_direct_entry_is_kept_however_long_the_device_is_away() {
    let mut e = Enumerator::with_backend(ScriptedBackend::new(Vec::new()));
    let direct = CacheKey::DirectSerial("d9:d2:69:b2:4b:96".to_string());
    e.cache.insert(direct.clone(), cache_entry());
    let nobody = HashSet::new();

    // Well past the grace: a keyboard on another computer.
    for _ in 0..(CACHE_MISS_GRACE * 10) {
        e.evict_unseen(&nobody, &nobody);
    }

    // The serial names the device, so nothing else can claim what it memoizes,
    // and keeping it is what lets the device come back without re-answering.
    assert!(e.cache.contains_key(&direct));
}

#[test]
fn a_serial_keyed_direct_entry_is_fresh_when_its_device_returns_from_a_long_absence() {
    let mut e = Enumerator::with_backend(ScriptedBackend::new(Vec::new()));
    let direct = CacheKey::DirectSerial("d9:d2:69:b2:4b:96".to_string());
    let mut entry = cache_entry();
    entry.probed_at = Instant::now()
        .checked_sub(REFRESH_INTERVAL * 10)
        .expect("the monotonic clock reaches back past the refresh interval");
    e.cache.insert(direct.clone(), entry);

    e.evict_unseen(&HashSet::new(), &HashSet::new());

    // Served from cache on return; the regular refresh re-walks it later.
    let kept = e.cache.get(&direct).expect("serial-keyed entry kept");
    assert!(!is_stale(kept, Instant::now()));
}

#[test]
fn being_seen_resets_the_miss_counter() {
    let mut e = Enumerator::with_backend(ScriptedBackend::new(Vec::new()));
    let key = CacheKey::Bolt { unit_id: [9; 4] };
    e.cache.insert(key.clone(), cache_entry());
    let nobody = HashSet::new();
    let seen: HashSet<CacheKey> = std::iter::once(key.clone()).collect();
    e.evict_unseen(&nobody, &nobody); // miss 1
    e.evict_unseen(&seen, &nobody); // seen → counter reset
    for _ in 0..CACHE_MISS_GRACE {
        e.evict_unseen(&nobody, &nobody);
    }
    assert!(
        e.cache.contains_key(&key),
        "counter reset by a sighting, so still within grace"
    );
}

#[test]
fn cached_probe_is_reused_until_refresh_interval() {
    let probed_at = Instant::now();
    let cached = Cached {
        probe: ProbedFeatures::default(),
        battery: None,
        events: EventFeatureIndices::default(),
        probed_at,
    };
    assert!(!is_stale(&cached, probed_at), "same instant is fresh");
    assert!(
        !is_stale(&cached, probed_at + Duration::from_secs(29)),
        "just under the window is still fresh"
    );
    assert!(
        !is_stale(&cached, probed_at + REFRESH_INTERVAL),
        "the window is inclusive: this exact age is still reused"
    );
    assert!(
        is_stale(
            &cached,
            probed_at + REFRESH_INTERVAL + Duration::from_nanos(1)
        ),
        "the probe is stale only after the window"
    );
}

#[test]
fn unifying_cache_hits_use_only_the_battery_refresh_budget() {
    let cached = cache_entry();
    let timeouts = &ProbeTimeouts::DEFAULT;
    assert_eq!(
        unifying_probe_budget(Some(&cached), cached.probed_at, timeouts),
        UNIFYING_CACHED_SLOT_PROBE_TIMEOUT
    );
    assert_eq!(
        unifying_probe_budget(Some(&cached), cached.probed_at + REFRESH_INTERVAL, timeouts),
        UNIFYING_CACHED_SLOT_PROBE_TIMEOUT,
        "an entry aged exactly one refresh interval is still a cache hit"
    );
    assert_eq!(
        unifying_probe_budget(
            Some(&cached),
            cached.probed_at + REFRESH_INTERVAL + Duration::from_nanos(1),
            timeouts,
        ),
        UNIFYING_SLOT_PROBE_TIMEOUT,
        "stale entries still get enough time for a full feature walk"
    );
    assert_eq!(
        unifying_probe_budget(None, Instant::now(), timeouts),
        UNIFYING_SLOT_PROBE_TIMEOUT,
        "first sight still gets the full feature-walk budget"
    );
}

#[test]
fn live_cached_channel_survives_a_transient_enumeration_gap() {
    let enumerated = std::collections::HashSet::from([1_u8]);
    let cached_channels = [(1_u8, true), (2_u8, true), (3_u8, false)];
    let retained = retained_nodes(&enumerated, cached_channels);
    assert!(retained.contains(&1));
    assert!(retained.contains(&2));
    assert!(!retained.contains(&3));
    assert_eq!(retained, std::collections::HashSet::from([1, 2]));
}

#[tokio::test]
async fn successful_channel_open_resets_eviction_but_not_inventory_grace() {
    let info = scripted_node_info("replacement");
    let node = info.id.clone();
    let backend = ScriptedBackend::new(vec![(info.clone(), ScriptedOpen::UnresponsiveHidpp)]);
    let mut enumerator = Enumerator::with_backend(backend.clone());
    enumerator
        .ledger
        .settle(&node, true, Some(inventory(&[1]).remove(0)));
    let mut expired = None;
    for _ in 0..8 {
        expired = enumerator.ledger.settle(&node, false, None).inventory;
    }
    assert!(
        expired.is_none(),
        "retirement ticks must eventually stop publishing stale inventory"
    );

    let prepared = enumerator.prepare_nodes(backend.as_ref(), vec![info]).await;
    assert_eq!(prepared.active.len(), 1, "the replacement must open");
    let first_incomplete_probe = enumerator.ledger.settle_arrival_replay_failure(&node);

    assert!(!first_incomplete_probe.evict_channel);
    assert!(first_incomplete_probe.inventory.is_none());
}

/// A backend whose `open_hidpp` sleeps for a fixed delay before failing,
/// standing in for a slow BLE-direct connection handshake.
struct SlowOpenBackend {
    delay: Duration,
}

#[hidpp::async_trait]
impl crate::backend::HidBackend for SlowOpenBackend {
    async fn enumerate(&self) -> Result<Vec<NodeInfo>, crate::backend::BackendError> {
        Ok(Vec::new())
    }

    async fn enumerate_hidpp(&self) -> Result<Vec<NodeInfo>, crate::backend::BackendError> {
        Ok(Vec::new())
    }

    async fn open_hidpp(
        &self,
        _node: &NodeInfo,
    ) -> Result<Option<Arc<hidpp::channel::HidppChannel>>, crate::backend::BackendError> {
        tokio::time::sleep(self.delay).await;
        Err(crate::backend::BackendError::Disconnected)
    }

    async fn open_raw_writer(
        &self,
        _node: &NodeInfo,
    ) -> Result<Box<dyn crate::backend::RawWriter>, crate::backend::BackendError> {
        Err(crate::backend::BackendError::Backend(
            "slow-open test backend has no raw writer".into(),
        ))
    }

    fn watch(&self) -> Result<crate::backend::HotplugStream, crate::backend::BackendError> {
        Ok(Box::new(futures_lite::stream::empty()))
    }
}

/// BLE-direct devices measured 10-15s each to open, fully serialized, because
/// `prepare_nodes` opened one node at a time. Three nodes that each take
/// `DELAY` to open must finish in about one `DELAY`, not three.
///
/// Uses paused virtual time instead of a wall-clock ceiling: `SlowOpenBackend`
/// sleeps via `tokio::time::sleep`, so with time paused a serialized open
/// would need the runtime to auto-advance past three delays before this task
/// can resume, while a concurrent one only needs one — proving the property
/// directly rather than measuring scheduler latency, which a loaded CI
/// runner can blow through even when every open genuinely ran concurrently.
#[tokio::test(start_paused = true)]
async fn opening_multiple_nodes_does_not_serialize_a_slow_connection_handshake() {
    const DELAY: Duration = Duration::from_millis(150);

    let backend = Arc::new(SlowOpenBackend { delay: DELAY });
    let mut enumerator = Enumerator::with_backend(backend.clone());
    let candidates = vec![
        scripted_node_info("slow-a"),
        scripted_node_info("slow-b"),
        scripted_node_info("slow-c"),
    ];

    let start = tokio::time::Instant::now();
    let prepared = tokio::time::timeout(DELAY * 2, enumerator.prepare_nodes(&*backend, candidates))
        .await
        .expect("three concurrent opens must resolve within two delays of virtual time");
    let elapsed = start.elapsed();

    assert_eq!(
        prepared.open_failures.len(),
        3,
        "every node's open must still be attempted and reported"
    );
    assert!(
        elapsed < DELAY * 2,
        "opening three {DELAY:?}-latency nodes advanced virtual time by {elapsed:?} — they were serialized"
    );
}

/// A backend can report the same node twice in one enumeration pass (a
/// duplicate hotplug event, a receiver re-listing a slot). The cache stays
/// empty for every occurrence of a not-yet-cached node until the concurrent
/// opens finish, so without deduplicating `to_open` the duplicate would be
/// opened a second time and end up live through two channels — the exact
/// split-delivery state `ChannelCache` exists to prevent.
#[tokio::test]
async fn a_duplicate_candidate_opens_only_one_channel() {
    let info = scripted_node_info("duplicated");
    let backend = ScriptedBackend::new(vec![(info.clone(), ScriptedOpen::UnresponsiveHidpp)]);
    let mut enumerator = Enumerator::with_backend(backend.clone());

    let prepared = enumerator
        .prepare_nodes(backend.as_ref(), vec![info.clone(), info])
        .await;

    assert_eq!(
        prepared.active.len(),
        1,
        "a duplicate candidate must only ever open one live channel"
    );
}

/// A backend whose single node opens into the channel that `open` builds. The
/// channel always reports itself connected, whatever it does with requests.
struct DirectChannelBackend {
    node: NodeInfo,
    open: fn() -> ScriptedRawHidChannel,
}

#[hidpp::async_trait]
impl crate::backend::HidBackend for DirectChannelBackend {
    async fn enumerate(&self) -> Result<Vec<NodeInfo>, crate::backend::BackendError> {
        Ok(vec![self.node.clone()])
    }

    async fn enumerate_hidpp(&self) -> Result<Vec<NodeInfo>, crate::backend::BackendError> {
        self.enumerate().await
    }

    async fn open_hidpp(
        &self,
        _node: &NodeInfo,
    ) -> Result<Option<Arc<hidpp::channel::HidppChannel>>, crate::backend::BackendError> {
        Ok(Some(scripted_channel((self.open)()).await))
    }

    async fn open_raw_writer(
        &self,
        _node: &NodeInfo,
    ) -> Result<Box<dyn crate::backend::RawWriter>, crate::backend::BackendError> {
        Err(crate::backend::BackendError::Backend(
            "direct-channel test backend has no raw writer".into(),
        ))
    }

    fn watch(&self) -> Result<crate::backend::HotplugStream, crate::backend::BackendError> {
        Ok(Box::new(futures_lite::stream::empty()))
    }
}

/// A handle that looks alive yet can no longer reach the device: it rejects
/// every write.
fn rejecting_writes() -> ScriptedRawHidChannel {
    ScriptedRawHidChannel::with_failing_writes(|_| None, |_| true).0
}

/// A device that answers the protocol ping and fails every other request, as
/// a lossy link presents a walk whose replies go missing.
fn answering_only_its_ping() -> ScriptedRawHidChannel {
    ScriptedRawHidChannel::with_responder(|request| {
        ping_reply(request).or_else(|| Some(feature_error(request, 0x01)))
    })
    .0
}

/// A wired HID++ 1.0 mouse (the G700s of #757): it reads HID++ 2.0's protocol
/// ping as sub id `0x00` and answers with HID++ 1.0's invalid-sub-id error.
fn answering_as_hidpp10() -> ScriptedRawHidChannel {
    ScriptedRawHidChannel::with_responder(|request| Some(register_error(request, 0x01))).0
}

/// A due cache entry for a pointer device whose last walk completed.
fn stale_pointer_entry() -> Cached {
    let mut last_good = cache_entry();
    last_good.probe.protocol = ProbedProtocol::Hidpp20 {
        capabilities: Capabilities {
            pointer: true,
            ..Capabilities::default()
        },
        incomplete: false,
    };
    last_good.probed_at = Instant::now()
        .checked_sub(REFRESH_INTERVAL + Duration::from_nanos(1))
        .expect("the refresh interval fits the monotonic clock");
    last_good
}

/// A stale direct-device probe whose refresh fails is a failed probe, not a
/// cache hit. Serving the cached answer in its place reports an unreachable
/// channel healthy, so the ledger never retires it and the device stays dead
/// until the agent restarts. A silent device takes the same branch once its
/// request times out; rejected writes reach it without waiting.
#[tokio::test]
async fn a_failed_direct_reprobe_retires_the_channel() {
    let info = scripted_node_info("unreachable-direct");
    let node = info.id.clone();
    let backend = Arc::new(DirectChannelBackend {
        node: info,
        open: rejecting_writes,
    });
    let mut enumerator = Enumerator::with_backend(backend);
    enumerator
        .cache
        .insert(CacheKey::Direct(node.clone()), stale_pointer_entry());

    enumerator
        .enumerate()
        .await
        .expect("a failed reprobe still settles");
    assert!(
        !enumerator.channels.is_retiring(&node),
        "one failed reprobe is not yet evidence of a dead channel"
    );
    enumerator
        .enumerate()
        .await
        .expect("a failed reprobe still settles");
    assert!(
        enumerator.channels.is_retiring(&node),
        "a second failed reprobe must retire the channel so it reopens"
    );
}

/// A stale direct-device probe whose device answers the ping but not the
/// re-read keeps its channel and stays published. The ping shows that the
/// channel delivers replies. Retiring it for a re-read that a lossy Bluetooth
/// link cut short ends control capture on a working device (#2107).
#[tokio::test]
async fn a_direct_reprobe_answered_by_its_ping_keeps_the_channel() {
    let info = scripted_node_info("lossy-direct");
    let node = info.id.clone();
    let backend = Arc::new(DirectChannelBackend {
        node: info,
        open: answering_only_its_ping,
    });
    let mut enumerator = Enumerator::with_backend(backend);
    enumerator
        .cache
        .insert(CacheKey::Direct(node.clone()), stale_pointer_entry());

    // Past the ledger's retirement threshold and its replay grace both.
    for pass in 1..=4 {
        let inventories = enumerator
            .enumerate()
            .await
            .expect("a reprobe the device answered settles");
        assert!(
            !enumerator.channels.is_retiring(&node),
            "pass {pass}: a channel that answers its ping is not dead"
        );
        assert_eq!(
            inventories.len(),
            1,
            "pass {pass}: the device stays published from its last complete walk"
        );
    }
    assert!(
        enumerator
            .cache
            .get(&CacheKey::Direct(node))
            .is_some_and(|entry| entry.probe.capabilities().is_some()),
        "the unfinished re-read must not replace the complete walk"
    );
}

/// A direct node that answers as HID++ 1.0 is settled, not failed. The answer
/// is definitive; counting it as a failure retired and reopened the channel
/// every few passes for as long as the device stayed plugged in (#2432).
#[tokio::test]
async fn a_direct_hidpp10_answer_keeps_the_channel() {
    let info = scripted_node_info("hidpp10-direct");
    let node = info.id.clone();
    let backend = Arc::new(DirectChannelBackend {
        node: info,
        open: answering_as_hidpp10,
    });
    let mut enumerator = Enumerator::with_backend(backend);

    // Past the ledger's retirement threshold and its replay grace both.
    for pass in 1..=4 {
        let inventories = enumerator
            .enumerate()
            .await
            .expect("a HID++ 1.0 answer settles");
        assert!(
            !enumerator.channels.is_retiring(&node),
            "pass {pass}: a channel whose device answered is not dead"
        );
        assert!(
            inventories.is_empty(),
            "pass {pass}: a direct HID++ 1.0 device is not listed yet (#2161)"
        );
    }
    assert!(
        !enumerator.cache.contains_key(&CacheKey::Direct(node)),
        "a rejected node is not cached"
    );
}
