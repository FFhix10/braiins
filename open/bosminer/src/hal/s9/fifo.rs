#[cfg(not(feature = "hctl_polling"))]
mod fifo_irq;
#[cfg(feature = "hctl_polling")]
mod fifo_poll;

/// This module provides thin API to access memory-mapped FPGA registers
/// and associated interrupts.
/// Exports FIFO management/send/receive and register access.
use std::marker::PhantomData;
use std::ops;
use std::time::Duration;

use s9_io::hchainio0;

use super::error::{self, ErrorKind};
use crate::hal;
use failure::ResultExt;

/// How long to wait for RX interrupt
const FIFO_READ_TIMEOUT: Duration = Duration::from_millis(5);

unsafe impl Send for HChainFifo {}
unsafe impl Sync for HChainFifo {}

/// Reference-like type holding a memory map created using UioMapping
/// Used to hold a memory mapping of IP core's register block
pub struct Mmap<T = u8> {
    map: uio::UioMapping,
    _marker: PhantomData<*const T>,
}

impl<T> Mmap<T> {
    /// Create a new memory mapping
    /// * `hashboard_idx` is the number of chain (numbering must match in device-tree)
    ///
    /// Marked `unsafe` because we can't check whether `T` is sized correctly and makes sense
    unsafe fn new(hashboard_idx: usize) -> error::Result<Self> {
        let (uio, uio_name) = open_ip_core_uio(hashboard_idx, "mem")?;
        let map = uio.map_mapping(0).with_context(|_| {
            ErrorKind::UioDevice(uio_name, "cannot map uio device".to_string())
        })?;

        Ok(Self {
            map,
            _marker: PhantomData,
        })
    }
}

impl<T> ops::Deref for Mmap<T> {
    type Target = T;

    fn deref(&self) -> &T {
        let ptr = self.map.ptr as *const T;
        unsafe { &*ptr }
    }
}

#[cfg(feature = "hctl_polling")]
pub struct HChainFifo {
    pub hash_chain_io: Mmap<hchainio0::RegisterBlock>,
}

#[cfg(not(feature = "hctl_polling"))]
pub struct HChainFifo {
    pub hash_chain_io: Mmap<hchainio0::RegisterBlock>,
    midstate_count: Option<usize>,
    work_tx_irq: uio::UioDevice,
    work_rx_irq: uio::UioDevice,
    cmd_rx_irq: uio::UioDevice,
}

fn open_ip_core_uio(
    hashboard_idx: usize,
    uio_type: &'static str,
) -> error::Result<(uio::UioDevice, String)> {
    let uio_name = format!("chain{}-{}", hashboard_idx - 1, uio_type);
    Ok((uio::UioDevice::open_by_name(&uio_name)?, uio_name))
}

/// Performs IRQ mapping of IP core's block
fn map_irq(hashboard_idx: usize, irq_type: &'static str) -> error::Result<uio::UioDevice> {
    let (uio, _uio_name) = open_ip_core_uio(hashboard_idx, irq_type)?;
    Ok(uio)
}

/// This is common implementation
impl HChainFifo {
    #[inline]
    pub fn is_work_tx_fifo_full(&self) -> bool {
        self.hash_chain_io.stat_reg.read().work_tx_full().bit()
    }

    #[inline]
    pub fn is_work_rx_fifo_empty(&self) -> bool {
        self.hash_chain_io.stat_reg.read().work_rx_empty().bit()
    }

    #[inline]
    pub fn is_cmd_rx_fifo_empty(&self) -> bool {
        self.hash_chain_io.stat_reg.read().cmd_rx_empty().bit()
    }

    #[inline]
    pub fn has_work_tx_space_for_one_job(&self) -> bool {
        self.hash_chain_io.stat_reg.read().irq_pend_work_tx().bit()
    }

    /// Wait fro command FIFO to become empty.
    /// Uses polling.
    pub fn wait_cmd_tx_fifo_empty(&self) {
        // TODO busy waiting has to be replaced once asynchronous processing is in place
        // jho: Not really, there's no IRQ for cmd tx fifo becomming "empty". The best we
        // can do is run this in a separate thread with timeout polling.
        // But we usually want to wait for cmd to be empty before we issue other commands,
        // so it's not really worth it to pursue asynchronicity vehemently in this case.
        while !self.hash_chain_io.stat_reg.read().cmd_tx_empty().bit() {}
    }

    pub fn enable_ip_core(&self) {
        self.hash_chain_io
            .ctrl_reg
            .modify(|_, w| w.enable().bit(true));
    }

    pub fn disable_ip_core(&self) {
        self.hash_chain_io
            .ctrl_reg
            .modify(|_, w| w.enable().bit(false));
    }

    pub fn set_ip_core_work_time(&self, work_time: u32) {
        self.hash_chain_io
            .work_time
            .write(|w| unsafe { w.bits(work_time) });
    }

    pub fn set_baud_clock_div(&self, baud_clock_div: u32) {
        self.hash_chain_io
            .baud_reg
            .write(|w| unsafe { w.bits(baud_clock_div) });
    }

    pub fn set_ip_core_midstate_count(&self, value: hchainio0::ctrl_reg::MIDSTATE_CNT_A) {
        self.hash_chain_io
            .ctrl_reg
            .modify(|_, w| w.midstate_cnt().variant(value));
    }

    /// TODO: have this method only for TX fifo type
    pub fn set_midstate_count(&mut self, midstate_count: usize) {
        self.midstate_count = Some(midstate_count);
    }

    pub fn send_work(
        &mut self,
        work: &hal::MiningWork,
        work_id: u32,
    ) -> Result<u32, failure::Error> {
        let hw_midstate_count = self.midstate_count.expect("midstate count was not set");
        let expected_midstate_count = work.midstates.len();
        assert_eq!(
            expected_midstate_count, hw_midstate_count,
            "Expected {} midstates, but S9 is configured for {} midstates!",
            expected_midstate_count, hw_midstate_count,
        );

        self.write_to_work_tx_fifo(work_id.to_le())?;
        self.write_to_work_tx_fifo(work.bits().to_le())?;
        self.write_to_work_tx_fifo(work.ntime.to_le())?;
        self.write_to_work_tx_fifo(work.merkle_root_tail().to_le())?;

        for mid in work.midstates.iter() {
            for midstate_word in mid.state.words::<u32>().rev() {
                self.write_to_work_tx_fifo(midstate_word.to_be())?;
            }
        }
        Ok(work_id)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Index of chain for testing (must exist and be defined in DTS)
    const TEST_CHAIN_INDEX: usize = 8;

    /// Try opening UIO device.
    /// This test needs properly configured UIO devices for hash-chain 8 in
    /// device-tree so that we have something to open.
    #[test]
    fn test_lookup_uio() {
        let name = String::from("chain7-mem");
        uio::UioDevice::open_by_name(&name).unwrap();
    }

    /// Try opening non-existent UIO device.
    #[test]
    fn test_lookup_uio_notfound() {
        let name = String::from("chain7-nonsense");
        let uio = uio::UioDevice::open_by_name(&name);
        assert!(
            uio.is_err(),
            "Found UIO device {} that shouldn't really be there"
        );
    }

    /// Try mapping memory from UIO device.
    #[test]
    fn test_map_uio() {
        unsafe {
            let _m: Mmap<u8> = Mmap::new(TEST_CHAIN_INDEX).unwrap();
        }
    }

    /// Try to map memory twice.
    /// This is to check that the UioMapping Drop trait is working: Drop
    /// does perform unmap which drops the Uio fd lock.
    #[test]
    fn test_map_uio_twice_checklock() {
        unsafe {
            let _m: Mmap<u8> = Mmap::new(TEST_CHAIN_INDEX).unwrap();
            let _m: Mmap<u8> = Mmap::new(TEST_CHAIN_INDEX).unwrap();
        }
    }

    /// Test that we are able to construct HChainFifo instance
    #[test]
    fn test_fifo_construction() {
        let _fifo = HChainFifo::new(TEST_CHAIN_INDEX).expect("fifo construction failed");
    }

    /// Try to map IRQ.
    #[test]
    fn test_map_irq() {
        map_irq(TEST_CHAIN_INDEX, "cmd-rx").unwrap();
    }

    /// Test that we get IRQ.
    /// Test it on empty tx queue (IRQ always asserted).
    #[test]
    fn test_get_irq() {
        let irq = map_irq(TEST_CHAIN_INDEX, "work-tx").unwrap();
        irq.irq_enable().unwrap();
        let res = irq.irq_wait_timeout(FIFO_READ_TIMEOUT);
        assert!(res.unwrap().is_some(), "expected interrupt");
    }

    /// Test that we get timeout when waiting for IRQ.
    /// Test it on empty rx queue (IRQ always deasserted).
    #[test]
    fn test_get_irq_timeout() {
        let mut fifo = HChainFifo::new(TEST_CHAIN_INDEX).expect("fifo construction failed");
        // fifo initialization flushes all received responses
        fifo.init().expect("fifo initialization failed");
        drop(fifo);
        // work rx fifo now shouldn't get any interrupts (it's empty)
        let irq = map_irq(TEST_CHAIN_INDEX, "work-rx").unwrap();
        irq.irq_enable().unwrap();
        let res = irq.irq_wait_timeout(FIFO_READ_TIMEOUT);
        assert!(res.unwrap().is_none(), "expected timeout");
    }
}
