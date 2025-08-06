// Copyright (c) 2019-2025 Provable Inc.
// This file is part of the snarkVM library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:

// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;

/// The stake required to land one solution per epoch at various points in time.
///
/// Each entry represents a threshold where, starting from the given timestamp,
/// a prover must have at least the specified amount of stake (in microcredits) to land one solution.
///
/// A prover with `n * stake` may land up to `n` solutions per epoch.
///
/// Format: `(timestamp, stake_required_per_solution)`
pub const STAKE_REQUIREMENTS_PER_SOLUTION: [(i64, u64); 9] = [
    (1754006399i64, 100_000_000_000u64),   /* 2025-07-31 23:59:59 UTC */
    (1761955199i64, 250_000_000_000u64),   /* 2025-10-31 23:59:59 UTC */
    (1769903999i64, 500_000_000_000u64),   /* 2026-01-31 23:59:59 UTC */
    (1777593599i64, 750_000_000_000u64),   /* 2026-04-30 23:59:59 UTC */
    (1785542399i64, 1_000_000_000_000u64), /* 2026-07-31 23:59:59 UTC */
    (1793491199i64, 1_250_000_000_000u64), /* 2026-10-31 23:59:59 UTC */
    (1801439999i64, 1_500_000_000_000u64), /* 2027-01-31 23:59:59 UTC */
    (1809129599i64, 2_000_000_000_000u64), /* 2027-04-30 23:59:59 UTC */
    (1817078399i64, 2_500_000_000_000u64), /* 2027-07-31 23:59:59 UTC */
];

/// Returns the maximum number of allowed solutions per epoch based on the provided stake and timestamp.
pub fn maximum_allowed_solutions_per_epoch(prover_stake: u64, current_time: i64) -> u64 {
    // If the block height is earlier than the starting enforcement, do not restrict the maximum number of solutions per epoch.
    if current_time < STAKE_REQUIREMENTS_PER_SOLUTION.first().map(|(t, _)| *t).unwrap_or(i64::MAX) {
        return u64::MAX;
    }

    // Find the minimum stake required for one solution per epoch.
    let minimum_stake_per_solution_per_epoch =
        match STAKE_REQUIREMENTS_PER_SOLUTION.binary_search_by_key(&current_time, |(t, _)| *t) {
            // If a stake limit was found at this height, return it.
            Ok(index) => STAKE_REQUIREMENTS_PER_SOLUTION[index].1,
            // If the specified height was not found, determine which limit to return.
            Err(index) => STAKE_REQUIREMENTS_PER_SOLUTION[index.saturating_sub(1)].1,
        };

    // Return the number of allowed solutions per epoch.
    prover_stake.saturating_div(minimum_stake_per_solution_per_epoch)
}

impl<N: Network, C: ConsensusStorage<N>> Ledger<N, C> {
    /// Returns the number of remaining solutions a prover can submit in the current epoch.
    pub fn num_remaining_solutions(&self, prover_address: &Address<N>, additional_solutions_in_block: u64) -> u64 {
        // Fetch the prover's stake.
        let prover_stake = self.get_bonded_amount(prover_address).unwrap_or(0);

        // Determine the maximum number of solutions allowed based on this prover's stake.
        let maximum_allowed_solutions = maximum_allowed_solutions_per_epoch(prover_stake, self.latest_timestamp());

        // Fetch the number of solutions the prover has earned rewards for in the current epoch.
        let prover_num_solutions_in_epoch = *self.epoch_provers_cache.read().get(prover_address).unwrap_or(&0);

        // Calculate the total number of solutions the prover has submitted in the current epoch including the current block.
        let num_solutions = (prover_num_solutions_in_epoch as u64).saturating_add(additional_solutions_in_block);

        // Return the number of remaining solutions.
        maximum_allowed_solutions.saturating_sub(num_solutions)
    }

    /// Returns `true` if the given prover address has reached their solution limit for the current epoch.
    pub fn is_solution_limit_reached(&self, prover_address: &Address<N>, additional_solutions_in_block: u64) -> bool {
        // Calculate the number of remaining solutions for the prover.
        let num_remaining_solutions = self.num_remaining_solutions(prover_address, additional_solutions_in_block);

        // If the number of remaining solutions is zero, the limit is reached.
        num_remaining_solutions == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITERATIONS: u64 = 100;

    #[test]
    fn test_solution_limit_per_epoch() {
        let mut rng = TestRng::default();

        for _ in 0..ITERATIONS {
            for window in STAKE_REQUIREMENTS_PER_SOLUTION.windows(2) {
                let (prev_time, stake_per_solution) = window[0];
                let (next_time, _) = window[1];

                // Choose a time strictly between the steps.
                let timestamp = rng.gen_range(prev_time..next_time);
                // Generate a random prover stake.
                let prover_stake: u64 = rng.r#gen();
                let expected_num_solutions = prover_stake / stake_per_solution;

                assert_eq!(maximum_allowed_solutions_per_epoch(prover_stake, timestamp), expected_num_solutions,);
            }
        }
    }

    #[test]
    fn test_solution_limit_before_enforcement() {
        let mut rng = TestRng::default();

        // Fetch the first timestamp from the table.
        let first_timestamp = STAKE_REQUIREMENTS_PER_SOLUTION.first().unwrap().0;
        let time_before_first = first_timestamp - 1;

        // Check that before enforcement, the number of solutions is unrestricted even without prover stake.
        let prover_stake = 0;
        assert_eq!(maximum_allowed_solutions_per_epoch(prover_stake, time_before_first), u64::MAX);

        // Check that before enforcement, the number of solutions is unrestricted for any prover stake.
        for _ in 0..ITERATIONS {
            assert_eq!(maximum_allowed_solutions_per_epoch(rng.r#gen(), rng.gen_range(0..time_before_first)), u64::MAX);
        }
    }

    #[test]
    fn test_solution_limit_after_final_timestamp() {
        let mut rng = TestRng::default();
        let (last_timestamp, stake_per_solution) = *STAKE_REQUIREMENTS_PER_SOLUTION.last().unwrap();

        // Check that all timestamps after the last one are treated as the last one.
        for _ in 0..ITERATIONS {
            let prover_stake: u64 = rng.r#gen();
            let time_after_last = rng.gen_range(last_timestamp..i64::MAX);
            let expected_num_solutions = prover_stake / stake_per_solution;

            assert_eq!(maximum_allowed_solutions_per_epoch(prover_stake, time_after_last), expected_num_solutions);
        }
    }

    #[test]
    fn test_solution_limit_exact_timestamps() {
        let mut rng = TestRng::default();
        // Check that the maximum allowed solutions per epoch is correct for each timestamp in the table.
        for &(timestamp, stake_per_solution) in STAKE_REQUIREMENTS_PER_SOLUTION.iter() {
            let expected_num_solutions = rng.gen_range(1..=100);
            let prover_stake = expected_num_solutions * stake_per_solution;

            assert_eq!(maximum_allowed_solutions_per_epoch(prover_stake, timestamp), expected_num_solutions,);
        }
    }
}
