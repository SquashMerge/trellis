//! Logical live-expansion reservations; distinct from cumulative row and lookup work.
use crate::scip_raw::ScipCaptureError;
use anyhow::Result;
use std::{cell::Cell, rc::Rc};

pub(super) struct LiveBudget {
    limit: u64,
    used: Cell<u64>,
    high_water: Cell<u64>,
}

impl LiveBudget {
    pub(super) fn new(limit: u64) -> Rc<Self> {
        Rc::new(Self {
            limit,
            used: Cell::new(0),
            high_water: Cell::new(0),
        })
    }

    pub(super) fn reserve(self: &Rc<Self>, bytes: u64) -> Result<Reservation> {
        let mut reservation = Reservation {
            budget: self.clone(),
            bytes: 0,
        };
        reservation.grow(bytes)?;
        Ok(reservation)
    }

    pub(super) fn high_water(&self) -> u64 {
        self.high_water.get()
    }
}

pub(in crate::semantic_import) struct Reservation {
    budget: Rc<LiveBudget>,
    bytes: u64,
}

impl Reservation {
    /// Move an existing owner's charge without changing aggregate live bytes. Moving more than
    /// this guard holds, or into another budget, is an accounting defect, reported typed.
    pub(super) fn transfer_to(&mut self, other: &mut Self, bytes: u64) -> Result<()> {
        anyhow::ensure!(
            Rc::ptr_eq(&self.budget, &other.budget) && bytes <= self.bytes,
            "live reservation transfer exceeds its owner"
        );
        self.bytes -= bytes;
        other.bytes += bytes;
        Ok(())
    }

    /// The bytes this guard holds.
    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Release storage only after its owner has actually dropped or moved to another guard.
    pub(super) fn release(&mut self, bytes: u64) -> Result<()> {
        anyhow::ensure!(
            bytes <= self.bytes,
            "live reservation release exceeds its owner"
        );
        self.bytes -= bytes;
        self.budget.used.set(self.budget.used.get() - bytes);
        Ok(())
    }

    pub(super) fn grow(&mut self, bytes: u64) -> Result<()> {
        let used = self
            .budget
            .used
            .get()
            .checked_add(bytes)
            .filter(|used| *used <= self.budget.limit)
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
        // The reservation is a subset of used, so the checked total also bounds this sum.
        self.bytes += bytes;
        self.budget.used.set(used);
        self.budget
            .high_water
            .set(self.budget.high_water.get().max(used));
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.set(self.budget.used.get() - self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simultaneous_owners_share_the_limit_and_release_on_drop() {
        let budget = LiveBudget::new(10);
        let first = budget.reserve(6).unwrap();
        let mut second = budget.reserve(4).unwrap();
        let error = second.grow(1).unwrap_err();
        assert!(matches!(error.downcast_ref::<ScipCaptureError>(),
            Some(ScipCaptureError::LimitExceeded(name)) if name == "unit expansion"));
        assert_eq!(budget.used.get(), 10);
        drop(first);
        second.grow(6).unwrap();
        assert_eq!(budget.used.get(), 10);
        drop(second);
        assert_eq!(budget.used.get(), 0);
        assert_eq!(budget.high_water(), 10);
    }

    #[test]
    fn moving_a_charge_does_not_require_extra_capacity() {
        let budget = LiveBudget::new(10);
        let mut retained = budget.reserve(10).unwrap();
        let mut scratch = budget.reserve(0).unwrap();
        retained.transfer_to(&mut scratch, 8).unwrap();
        assert_eq!(budget.used.get(), 10);
        assert_eq!(budget.high_water(), 10);
        drop(retained);
        assert_eq!(budget.used.get(), 8);
        drop(scratch);
        assert_eq!(budget.used.get(), 0);
    }

    #[test]
    fn overflow_fails_without_changing_the_reservation() {
        let budget = LiveBudget::new(u64::MAX);
        let first = budget.reserve(u64::MAX).unwrap();
        assert!(budget.reserve(1).is_err());
        assert_eq!(budget.used.get(), u64::MAX);
        drop(first);
        assert_eq!(budget.used.get(), 0);
    }

    #[test]
    fn overdrawn_transfer_and_release_are_typed_and_change_nothing() {
        let budget = LiveBudget::new(10);
        let other_budget = LiveBudget::new(10);
        let mut retained = budget.reserve(4).unwrap();
        let mut scratch = budget.reserve(0).unwrap();
        let mut foreign = other_budget.reserve(0).unwrap();
        assert!(retained.transfer_to(&mut scratch, 5).is_err());
        assert!(retained.transfer_to(&mut foreign, 1).is_err());
        assert!(retained.release(5).is_err());
        assert_eq!((retained.bytes, scratch.bytes, foreign.bytes), (4, 0, 0));
        assert_eq!(budget.used.get(), 4);
        assert_eq!(other_budget.used.get(), 0);
        retained.release(4).unwrap();
        assert_eq!(budget.used.get(), 0);
    }
}
