//! Startup-only head sources. They never activate registry checkouts.
use std::{sync::Arc, time::Duration};

use anyhow::Result;

use super::{
    search_fill::FillSource,
    search_head::{HeadChange, HeadUpdate},
};
use crate::{
    model::IndexPin,
    store::{HeadSearchCards, RecentSearchCheckout, Store, topology::TopologyRoots},
};

// Owner policy: #358, issuecomment-6446057555.
const STARTUP_CHECKOUT_LIMIT: usize = 4;
const STARTUP_RECENT_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

struct StartupSource {
    roots: TopologyRoots,
    checkout: RecentSearchCheckout,
}

impl FillSource for StartupSource {
    fn is_startup(&self) -> bool {
        true
    }

    fn root_key(&self) -> Option<String> {
        Some(self.checkout.root_key.clone())
    }

    fn head(
        &self,
        _previous: Option<IndexPin>,
        admit: &mut dyn FnMut(usize, usize, bool) -> Result<()>,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Option<HeadUpdate>> {
        let (pin, cards) = Store::read_existing_search_head_admitted(
            &self.roots,
            &self.checkout,
            &mut || check(),
            &mut |count| admit(count, 0, true),
        )?;
        Ok(Some(HeadUpdate {
            pin,
            declarations: cards.len(),
            change: HeadChange::Full(cards),
        }))
    }

    fn page(&self, pin: Option<IndexPin>, _after: Option<&str>) -> Result<HeadSearchCards> {
        let current = match pin {
            Some(pin) => pin,
            None => Store::read_existing_search_pin(&self.roots, &self.checkout)?,
        };
        // Startup warms resident cache rows. Active checkout ownership fills
        // missing document vectors using its normal bounded card pages.
        Ok(HeadSearchCards {
            pin: current,
            cards: Vec::new(),
        })
    }
}

/// Called only by the fill driver, outside its inbox lock. Optional startup
/// discovery never delays daemon socket acceptance or activates a checkout.
pub(super) fn discover_sources(roots: TopologyRoots) -> Vec<Arc<dyn FillSource>> {
    #[cfg(feature = "test-support")]
    let mut discovery_hook = std::env::var_os("TRELLIS_TEST_SEARCH_STARTUP_SOCKET").map(|path| {
        use std::io::{Read, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(path)
            .expect("connect search startup discovery fixture");
        stream
            .set_read_timeout(Some(crate::testing::HANG_BACKSTOP))
            .unwrap();
        stream
            .set_write_timeout(Some(crate::testing::HANG_BACKSTOP))
            .unwrap();
        stream
            .write_all(&[1])
            .expect("report search startup discovery");
        stream
            .read_exact(&mut [0])
            .expect("release search startup discovery");
        stream
    });
    let checkouts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|now| {
            Store::recent_search_checkouts(
                &roots,
                now.as_secs(),
                STARTUP_RECENT_AGE,
                STARTUP_CHECKOUT_LIMIT,
            )
            .ok()
        })
        .unwrap_or_default();
    let sources = checkouts
        .into_iter()
        .map(|checkout| {
            Arc::new(StartupSource {
                roots: roots.clone(),
                checkout,
            }) as Arc<dyn FillSource>
        })
        .collect();
    #[cfg(feature = "test-support")]
    if let Some(stream) = &mut discovery_hook {
        use std::io::Write;
        stream
            .write_all(&[2])
            .expect("report search startup discovered");
    }
    sources
}

#[cfg(test)]
#[path = "search_startup_tests.rs"]
mod tests;
