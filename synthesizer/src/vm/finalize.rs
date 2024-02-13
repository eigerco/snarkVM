// Copyright (C) 2019-2023 Aleo Systems Inc.
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

use ledger_committee::{MAX_DELEGATORS, MIN_DELEGATOR_STAKE, MIN_VALIDATOR_STAKE};

impl<N: Network, C: ConsensusStorage<N>> VM<N, C> {
    /// Speculates on the given list of transactions in the VM.
    ///
    /// Returns the confirmed transactions, aborted transaction IDs,
    /// and finalize operations from pre-ratify and post-ratify.
    ///
    /// Note: This method is used to create a new block (including the genesis block).
    ///   - If `coinbase_reward = None`, then the `ratifications` will not be modified.
    ///   - If `coinbase_reward = Some(coinbase_reward)`, then the method will append a
    ///     `Ratify::BlockReward(block_reward)` and `Ratify::PuzzleReward(puzzle_reward)`
    ///     to the front of the `ratifications` list.
    #[inline]
    pub fn speculate<'a>(
        &self,
        state: FinalizeGlobalState,
        coinbase_reward: Option<u64>,
        candidate_ratifications: Vec<Ratify<N>>,
        candidate_solutions: &Solutions<N>,
        candidate_transactions: impl ExactSizeIterator<Item = &'a Transaction<N>>,
    ) -> Result<(Ratifications<N>, Transactions<N>, Vec<N::TransactionID>, Vec<FinalizeOperation<N>>)> {
        let timer = timer!("VM::speculate");

        // Performs a **dry-run** over the list of ratifications, solutions, and transactions.
        let (ratifications, confirmed_transactions, aborted_transactions, ratified_finalize_operations) = self
            .atomic_speculate(
                state,
                coinbase_reward,
                candidate_ratifications,
                candidate_solutions,
                candidate_transactions,
            )?;

        // Convert the aborted transactions into aborted transaction IDs.
        let mut aborted_transaction_ids = Vec::with_capacity(aborted_transactions.len());
        for (tx, error) in aborted_transactions {
            warn!("Speculation safely aborted a transaction - {error} ({})", tx.id());
            aborted_transaction_ids.push(tx.id());
        }

        finish!(timer, "Finished dry-run of the transactions");

        // Return the ratifications, confirmed transactions, aborted transaction IDs, and ratified finalize operations.
        Ok((
            ratifications,
            confirmed_transactions.into_iter().collect(),
            aborted_transaction_ids,
            ratified_finalize_operations,
        ))
    }

    /// Checks the speculation on the given transactions in the VM.
    ///
    /// Returns the finalize operations from pre-ratify and post-ratify.
    #[inline]
    pub fn check_speculate(
        &self,
        state: FinalizeGlobalState,
        ratifications: &Ratifications<N>,
        solutions: &Solutions<N>,
        transactions: &Transactions<N>,
    ) -> Result<Vec<FinalizeOperation<N>>> {
        let timer = timer!("VM::check_speculate");

        // Reconstruct the candidate ratifications to verify the speculation.
        let candidate_ratifications = ratifications.iter().cloned().collect::<Vec<_>>();
        // Reconstruct the unconfirmed transactions to verify the speculation.
        let candidate_transactions =
            transactions.iter().map(|confirmed| confirmed.to_unconfirmed_transaction()).collect::<Result<Vec<_>>>()?;

        // Performs a **dry-run** over the list of ratifications, solutions, and transactions.
        let (speculate_ratifications, confirmed_transactions, aborted_transactions, ratified_finalize_operations) =
            self.atomic_speculate(state, None, candidate_ratifications, solutions, candidate_transactions.iter())?;

        // Ensure the ratifications after speculation match.
        if ratifications != &speculate_ratifications {
            bail!("The ratifications after speculation do not match the ratifications in the block");
        }
        // Ensure the transactions after speculation match.
        if transactions != &confirmed_transactions.into_iter().collect() {
            bail!("The transactions after speculation do not match the transactions in the block");
        }
        // Ensure there are no aborted transaction IDs from this speculation.
        // Note: There should be no aborted transactions, because we are checking a block,
        // where any aborted transactions should be in the aborted transaction ID list, not in transactions.
        ensure!(aborted_transactions.is_empty(), "Aborted transactions found in the block (from speculation)");

        finish!(timer, "Finished dry-run of the transactions");

        // Return the ratified finalize operations.
        Ok(ratified_finalize_operations)
    }

    /// Finalizes the given transactions into the VM.
    ///
    /// Returns the finalize operations from pre-ratify and post-ratify.
    #[inline]
    pub fn finalize(
        &self,
        state: FinalizeGlobalState,
        ratifications: &Ratifications<N>,
        solutions: &Solutions<N>,
        transactions: &Transactions<N>,
    ) -> Result<Vec<FinalizeOperation<N>>> {
        let timer = timer!("VM::finalize");

        // Performs a **real-run** of finalize over the list of ratifications, solutions, and transactions.
        let ratified_finalize_operations = self.atomic_finalize(state, ratifications, solutions, transactions)?;

        finish!(timer, "Finished real-run of finalize");
        Ok(ratified_finalize_operations)
    }
}

impl<N: Network, C: ConsensusStorage<N>> VM<N, C> {
    /// The maximum number of confirmed transactions allowed in a block.
    #[cfg(not(any(test, feature = "test")))]
    pub const MAXIMUM_CONFIRMED_TRANSACTIONS: usize = Transactions::<N>::MAX_TRANSACTIONS;
    /// The maximum number of confirmed transactions allowed in a block.
    /// This is set to a deliberately low value (8) for testing purposes only.
    #[cfg(any(test, feature = "test"))]
    pub const MAXIMUM_CONFIRMED_TRANSACTIONS: usize = 8;

    /// Performs atomic speculation over a list of transactions.
    ///
    /// Returns the ratifications, confirmed transactions, aborted transactions,
    /// and finalize operations from pre-ratify and post-ratify.
    ///
    /// Note: This method is used by `VM::speculate` and `VM::check_speculate`.
    ///   - If `coinbase_reward = None`, then the `ratifications` will not be modified.
    ///   - If `coinbase_reward = Some(coinbase_reward)`, then the method will append a
    ///     `Ratify::BlockReward(block_reward)` and `Ratify::PuzzleReward(puzzle_reward)`
    ///     to the front of the `ratifications` list.
    fn atomic_speculate<'a>(
        &self,
        state: FinalizeGlobalState,
        coinbase_reward: Option<u64>,
        ratifications: Vec<Ratify<N>>,
        solutions: &Solutions<N>,
        transactions: impl ExactSizeIterator<Item = &'a Transaction<N>>,
    ) -> Result<(
        Ratifications<N>,
        Vec<ConfirmedTransaction<N>>,
        Vec<(Transaction<N>, String)>,
        Vec<FinalizeOperation<N>>,
    )> {
        // Acquire the atomic lock, which is needed to ensure this function is not called concurrently
        // with other `atomic_finalize!` macro calls, which will cause a `bail!` to be triggered erroneously.
        // Note: This lock must be held for the entire scope of the call to `atomic_finalize!`.
        let _atomic_lock = self.atomic_lock.lock();

        let timer = timer!("VM::atomic_speculate");

        // Retrieve the number of solutions.
        let num_solutions = solutions.len();
        // Retrieve the number of transactions.
        let num_transactions = transactions.len();

        // Perform the finalize operation on the preset finalize mode.
        atomic_finalize!(self.finalize_store(), FinalizeMode::DryRun, {
            // Ensure the number of solutions does not exceed the maximum.
            if num_solutions > Solutions::<N>::MAX_ABORTED_SOLUTIONS {
                // Note: This will abort the entire atomic batch.
                return Err(format!(
                    "Too many solutions in the block - {num_solutions} (max: {})",
                    Solutions::<N>::MAX_ABORTED_SOLUTIONS
                ));
            }

            // Ensure the number of transactions does not exceed the maximum.
            if num_transactions > Transactions::<N>::MAX_ABORTED_TRANSACTIONS {
                // Note: This will abort the entire atomic batch.
                return Err(format!(
                    "Too many transactions in the block - {num_transactions} (max: {})",
                    Transactions::<N>::MAX_ABORTED_TRANSACTIONS
                ));
            }

            // Initialize an iterator for ratifications before finalize.
            let pre_ratifications = ratifications.iter().filter(|r| match r {
                Ratify::Genesis(_, _, _) => true,
                Ratify::BlockReward(..) | Ratify::PuzzleReward(..) => false,
            });
            // Initialize an iterator for ratifications after finalize.
            let post_ratifications = ratifications.iter().filter(|r| match r {
                Ratify::Genesis(_, _, _) => false,
                Ratify::BlockReward(..) | Ratify::PuzzleReward(..) => true,
            });

            // Initialize a list of finalize operations.
            let mut ratified_finalize_operations = Vec::new();

            // Retrieve the finalize store.
            let store = self.finalize_store();

            /* Perform the ratifications before finalize. */

            match Self::atomic_pre_ratify(store, state, pre_ratifications) {
                // Store the finalize operations from the post-ratify.
                Ok(operations) => ratified_finalize_operations.extend(operations),
                // Note: This will abort the entire atomic batch.
                Err(e) => return Err(format!("Failed to pre-ratify - {e}")),
            }

            /* Perform the atomic finalize over the transactions. */

            // Acquire the write lock on the process.
            // Note: Due to the highly-sensitive nature of processing all `finalize` calls,
            // we choose to acquire the write lock for the entire duration of this atomic batch.
            let process = self.process.write();

            // Initialize a list of the confirmed transactions.
            let mut confirmed = Vec::with_capacity(num_transactions);
            // Initialize a list of the aborted transactions.
            let mut aborted = Vec::new();
            // Initialize a list of the successful deployments.
            let mut deployments = IndexSet::new();
            // Initialize a counter for the confirmed transaction index.
            let mut counter = 0u32;
            // Initialize a list of created transition IDs.
            let mut transition_ids: IndexSet<N::TransitionID> = IndexSet::new();
            // Initialize a list of spent input IDs.
            let mut input_ids: IndexSet<Field<N>> = IndexSet::new();
            // Initialize a list of created output IDs.
            let mut output_ids: IndexSet<Field<N>> = IndexSet::new();
            // Initialize the list of created transition public keys.
            let mut tpks: IndexSet<Group<N>> = IndexSet::new();

            // Finalize the transactions.
            'outer: for transaction in transactions {
                // Ensure the number of confirmed transactions does not exceed the maximum.
                // Upon reaching the maximum number of confirmed transactions, all remaining transactions are aborted.
                if confirmed.len() >= Self::MAXIMUM_CONFIRMED_TRANSACTIONS {
                    // Store the aborted transaction.
                    aborted.push((transaction.clone(), "Exceeds block transaction limit".to_string()));
                    // Continue to the next transaction.
                    continue 'outer;
                }

                // Ensure that the transaction is not producing a duplicate transition.
                for transition_id in transaction.transition_ids() {
                    // If the transition ID is already produced in this block or previous blocks, abort the transaction.
                    if transition_ids.contains(transition_id)
                        || self.transition_store().contains_transition_id(transition_id).unwrap_or(true)
                    {
                        // Store the aborted transaction.
                        aborted.push((transaction.clone(), format!("Duplicate transition {transition_id}")));
                        // Continue to the next transaction.
                        continue 'outer;
                    }
                }

                // Ensure that the transaction is not double-spending an input.
                for input_id in transaction.input_ids() {
                    // If the input ID is already spent in this block or previous blocks, abort the transaction.
                    if input_ids.contains(input_id)
                        || self.transition_store().contains_input_id(input_id).unwrap_or(true)
                    {
                        // Store the aborted transaction.
                        aborted.push((transaction.clone(), format!("Double-spending input {input_id}")));
                        // Continue to the next transaction.
                        continue 'outer;
                    }
                }

                // Ensure that the transaction is not producing a duplicate output.
                for output_id in transaction.output_ids() {
                    // If the output ID is already produced in this block or previous blocks, abort the transaction.
                    if output_ids.contains(output_id)
                        || self.transition_store().contains_output_id(output_id).unwrap_or(true)
                    {
                        // Store the aborted transaction.
                        aborted.push((transaction.clone(), format!("Duplicate output {output_id}")));
                        // Continue to the next transaction.
                        continue 'outer;
                    }
                }

                // // Ensure that the transaction is not producing a duplicate transition public key.
                // // Note that the tpk and tcm are corresponding, so a uniqueness check for just the tpk is sufficient.
                for tpk in transaction.transition_public_keys() {
                    // If the transition public key is already produced in this block or previous blocks, abort the transaction.
                    if tpks.contains(tpk) || self.transition_store().contains_tpk(tpk).unwrap_or(true) {
                        // Store the aborted transaction.
                        aborted.push((transaction.clone(), format!("Duplicate transition public key {tpk}")));
                        // Continue to the next transaction.
                        continue 'outer;
                    }
                }

                // Process the transaction in an isolated atomic batch.
                // - If the transaction succeeds, the finalize operations are stored.
                // - If the transaction fails, the atomic batch is aborted and no finalize operations are stored.
                let outcome = match transaction {
                    // The finalize operation here involves appending the 'stack',
                    // and adding the program to the finalize tree.
                    Transaction::Deploy(_, program_owner, deployment, fee) => {
                        // Define the closure for processing a rejected deployment.
                        let process_rejected_deployment =
                            |fee: &Fee<N>,
                             deployment: Deployment<N>|
                             -> Result<Result<ConfirmedTransaction<N>, String>> {
                                process
                                    .finalize_fee(state, store, fee)
                                    .and_then(|finalize| {
                                        Transaction::from_fee(fee.clone()).map(|fee_tx| (fee_tx, finalize))
                                    })
                                    .map(|(fee_tx, finalize)| {
                                        let rejected = Rejected::new_deployment(*program_owner, deployment);
                                        ConfirmedTransaction::rejected_deploy(counter, fee_tx, rejected, finalize)
                                            .map_err(|e| e.to_string())
                                    })
                            };

                        // Check if the program has already been deployed in this block.
                        match deployments.contains(deployment.program_id()) {
                            // If the program has already been deployed, construct the rejected deploy transaction.
                            true => match process_rejected_deployment(fee, *deployment.clone()) {
                                Ok(result) => result,
                                Err(error) => {
                                    // Note: On failure, skip this transaction, and continue speculation.
                                    #[cfg(debug_assertions)]
                                    eprintln!("Failed to finalize the fee in a rejected deploy - {error}");
                                    // Store the aborted transaction.
                                    aborted.push((transaction.clone(), error.to_string()));
                                    // Continue to the next transaction.
                                    continue 'outer;
                                }
                            },
                            // If the program has not yet been deployed, attempt to deploy it.
                            false => match process.finalize_deployment(state, store, deployment, fee) {
                                // Construct the accepted deploy transaction.
                                Ok((_, finalize)) => {
                                    // Add the program id to the list of deployments.
                                    deployments.insert(*deployment.program_id());
                                    ConfirmedTransaction::accepted_deploy(counter, transaction.clone(), finalize)
                                        .map_err(|e| e.to_string())
                                }
                                // Construct the rejected deploy transaction.
                                Err(_error) => match process_rejected_deployment(fee, *deployment.clone()) {
                                    Ok(result) => result,
                                    Err(error) => {
                                        // Note: On failure, skip this transaction, and continue speculation.
                                        #[cfg(debug_assertions)]
                                        eprintln!("Failed to finalize the fee in a rejected deploy - {error}");
                                        // Store the aborted transaction.
                                        aborted.push((transaction.clone(), error.to_string()));
                                        // Continue to the next transaction.
                                        continue 'outer;
                                    }
                                },
                            },
                        }
                    }
                    // The finalize operation here involves calling 'update_key_value',
                    // and update the respective leaves of the finalize tree.
                    Transaction::Execute(_, execution, fee) => {
                        match process.finalize_execution(state, store, execution, fee.as_ref()) {
                            // Construct the accepted execute transaction.
                            Ok(finalize) => {
                                ConfirmedTransaction::accepted_execute(counter, transaction.clone(), finalize)
                                    .map_err(|e| e.to_string())
                            }
                            // Construct the rejected execute transaction.
                            Err(_error) => match fee {
                                // Finalize the fee, to ensure it is valid.
                                Some(fee) => {
                                    match process.finalize_fee(state, store, fee).and_then(|finalize| {
                                        Transaction::from_fee(fee.clone()).map(|fee_tx| (fee_tx, finalize))
                                    }) {
                                        Ok((fee_tx, finalize)) => {
                                            // Construct the rejected execution.
                                            let rejected = Rejected::new_execution(execution.clone());
                                            // Construct the rejected execute transaction.
                                            ConfirmedTransaction::rejected_execute(counter, fee_tx, rejected, finalize)
                                                .map_err(|e| e.to_string())
                                        }
                                        Err(error) => {
                                            // Note: On failure, skip this transaction, and continue speculation.
                                            #[cfg(debug_assertions)]
                                            eprintln!("Failed to finalize the fee in a rejected execute - {error}");
                                            // Store the aborted transaction.
                                            aborted.push((transaction.clone(), error.to_string()));
                                            // Continue to the next transaction.
                                            continue 'outer;
                                        }
                                    }
                                }
                                // This is a foundational bug - the caller is violating protocol rules.
                                // Note: This will abort the entire atomic batch.
                                None => Err("Rejected execute transaction has no fee".to_string()),
                            },
                        }
                    }
                    // There are no finalize operations here.
                    // Note: This will abort the entire atomic batch.
                    Transaction::Fee(..) => Err("Cannot speculate on a fee transaction".to_string()),
                };
                lap!(timer, "Speculated on transaction '{}'", transaction.id());

                match outcome {
                    // If the transaction succeeded, store it and continue to the next transaction.
                    Ok(confirmed_transaction) => {
                        // Add the transition IDs to the set of produced transition IDs.
                        transition_ids.extend(confirmed_transaction.transaction().transition_ids());
                        // Add the input IDs to the set of spent input IDs.
                        input_ids.extend(confirmed_transaction.transaction().input_ids());
                        // Add the output IDs to the set of produced output IDs.
                        output_ids.extend(confirmed_transaction.transaction().output_ids());
                        // Add the transition public keys to the set of produced transition public keys.
                        tpks.extend(confirmed_transaction.transaction().transition_public_keys());
                        // Store the confirmed transaction.
                        confirmed.push(confirmed_transaction);
                        // Increment the transaction index counter.
                        counter = counter.saturating_add(1);
                    }
                    // If the transaction failed, abort the entire batch.
                    Err(error) => {
                        eprintln!("Critical bug in speculate: {error}\n\n{transaction}");
                        // Note: This will abort the entire atomic batch.
                        return Err(format!("Failed to speculate on transaction - {error}"));
                    }
                }
            }

            // Ensure all transactions were processed.
            if confirmed.len() + aborted.len() != num_transactions {
                // Note: This will abort the entire atomic batch.
                return Err("Not all transactions were processed in 'VM::atomic_speculate'".to_string());
            }

            /* Perform the ratifications after finalize. */

            // Prepare the reward ratifications, if any.
            let reward_ratifications = match coinbase_reward {
                // If the coinbase reward is `None`, then there are no reward ratifications.
                None => vec![],
                // If the coinbase reward is `Some(coinbase_reward)`, then we must compute the reward ratifications.
                Some(coinbase_reward) => {
                    // Calculate the transaction fees.
                    let Ok(transaction_fees) =
                        confirmed.iter().map(|tx| Ok(*tx.priority_fee_amount()?)).sum::<Result<u64>>()
                    else {
                        // Note: This will abort the entire atomic batch.
                        return Err("Failed to calculate the transaction fees during speculation".to_string());
                    };

                    // Compute the block reward.
                    let block_reward = ledger_block::block_reward(
                        N::STARTING_SUPPLY,
                        N::BLOCK_TIME,
                        coinbase_reward,
                        transaction_fees,
                    );
                    // Compute the puzzle reward.
                    let puzzle_reward = ledger_block::puzzle_reward(coinbase_reward);

                    // Output the reward ratifications.
                    vec![Ratify::BlockReward(block_reward), Ratify::PuzzleReward(puzzle_reward)]
                }
            };

            // Update the post-ratifications iterator.
            let post_ratifications = reward_ratifications.iter().chain(post_ratifications);

            // Process the post-ratifications.
            match Self::atomic_post_ratify(store, state, post_ratifications, solutions) {
                // Store the finalize operations from the post-ratify.
                Ok(operations) => ratified_finalize_operations.extend(operations),
                // Note: This will abort the entire atomic batch.
                Err(e) => return Err(format!("Failed to post-ratify - {e}")),
            }

            /* Construct the ratifications after speculation. */

            let Ok(ratifications) =
                Ratifications::try_from_iter(reward_ratifications.into_iter().chain(ratifications.into_iter()))
            else {
                // Note: This will abort the entire atomic batch.
                return Err("Failed to construct the ratifications after speculation".to_string());
            };

            finish!(timer);

            // On return, 'atomic_finalize!' will abort the batch, and return the ratifications,
            // confirmed & aborted transactions, and finalize operations from pre-ratify and post-ratify.
            Ok((ratifications, confirmed, aborted, ratified_finalize_operations))
        })
    }

    /// Performs atomic finalization over a list of transactions.
    ///
    /// Returns the finalize operations from pre-ratify and post-ratify.
    #[inline]
    fn atomic_finalize(
        &self,
        state: FinalizeGlobalState,
        ratifications: &Ratifications<N>,
        solutions: &Solutions<N>,
        transactions: &Transactions<N>,
    ) -> Result<Vec<FinalizeOperation<N>>> {
        // Acquire the atomic lock, which is needed to ensure this function is not called concurrently
        // with other `atomic_finalize!` macro calls, which will cause a `bail!` to be triggered erroneously.
        // Note: This lock must be held for the entire scope of the call to `atomic_finalize!`.
        let _atomic_lock = self.atomic_lock.lock();

        let timer = timer!("VM::atomic_finalize");

        // Perform the finalize operation on the preset finalize mode.
        atomic_finalize!(self.finalize_store(), FinalizeMode::RealRun, {
            // Initialize an iterator for ratifications before finalize.
            let pre_ratifications = ratifications.iter().filter(|r| match r {
                Ratify::Genesis(_, _, _) => true,
                Ratify::BlockReward(..) | Ratify::PuzzleReward(..) => false,
            });
            // Initialize an iterator for ratifications after finalize.
            let post_ratifications = ratifications.iter().filter(|r| match r {
                Ratify::Genesis(_, _, _) => false,
                Ratify::BlockReward(..) | Ratify::PuzzleReward(..) => true,
            });

            // Initialize a list of finalize operations.
            let mut ratified_finalize_operations = Vec::new();

            // Retrieve the finalize store.
            let store = self.finalize_store();

            /* Perform the ratifications before finalize. */

            match Self::atomic_pre_ratify(store, state, pre_ratifications) {
                // Store the finalize operations from the post-ratify.
                Ok(operations) => ratified_finalize_operations.extend(operations),
                // Note: This will abort the entire atomic batch.
                Err(e) => return Err(format!("Failed to pre-ratify - {e}")),
            }

            /* Perform the atomic finalize over the transactions. */

            // Acquire the write lock on the process.
            // Note: Due to the highly-sensitive nature of processing all `finalize` calls,
            // we choose to acquire the write lock for the entire duration of this atomic batch.
            let mut process = self.process.write();

            // Initialize a list for the deployed stacks.
            let mut stacks = Vec::new();

            // Finalize the transactions.
            for (index, transaction) in transactions.iter().enumerate() {
                // Convert the transaction index to a u32.
                // Note: On failure, this will abort the entire atomic batch.
                let index = u32::try_from(index).map_err(|_| "Failed to convert transaction index".to_string())?;
                // Ensure the index matches the expected index.
                if index != transaction.index() {
                    // Note: This will abort the entire atomic batch.
                    return Err(format!("Mismatch in {} transaction index", transaction.variant()));
                }
                // Process the transaction in an isolated atomic batch.
                // - If the transaction succeeds, the finalize operations are stored.
                // - If the transaction fails, the atomic batch is aborted and no finalize operations are stored.
                let outcome: Result<(), String> = match transaction {
                    ConfirmedTransaction::AcceptedDeploy(_, transaction, finalize) => {
                        // Extract the deployment and fee from the transaction.
                        let (deployment, fee) = match transaction {
                            Transaction::Deploy(_, _, deployment, fee) => (deployment, fee),
                            // Note: This will abort the entire atomic batch.
                            _ => return Err("Expected deploy transaction".to_string()),
                        };
                        // The finalize operation here involves appending the 'stack', and adding the program to the finalize tree.
                        match process.finalize_deployment(state, store, deployment, fee) {
                            // Ensure the finalize operations match the expected.
                            Ok((stack, finalize_operations)) => match finalize == &finalize_operations {
                                // Store the stack.
                                true => stacks.push(stack),
                                // Note: This will abort the entire atomic batch.
                                false => {
                                    return Err(format!(
                                        "Mismatch in finalize operations for an accepted deploy - (found: {finalize_operations:?}, expected: {finalize:?})"
                                    ));
                                }
                            },
                            // Note: This will abort the entire atomic batch.
                            Err(error) => {
                                return Err(format!("Failed to finalize an accepted deploy transaction - {error}"));
                            }
                        };
                        Ok(())
                    }
                    ConfirmedTransaction::AcceptedExecute(_, transaction, finalize) => {
                        // Extract the execution and fee from the transaction.
                        let (execution, fee) = match transaction {
                            Transaction::Execute(_, execution, fee) => (execution, fee),
                            // Note: This will abort the entire atomic batch.
                            _ => return Err("Expected execute transaction".to_string()),
                        };
                        // The finalize operation here involves calling 'update_key_value',
                        // and update the respective leaves of the finalize tree.
                        match process.finalize_execution(state, store, execution, fee.as_ref()) {
                            // Ensure the finalize operations match the expected.
                            Ok(finalize_operations) => {
                                if finalize != &finalize_operations {
                                    // Note: This will abort the entire atomic batch.
                                    return Err(format!(
                                        "Mismatch in finalize operations for an accepted execute - (found: {finalize_operations:?}, expected: {finalize:?})"
                                    ));
                                }
                            }
                            // Note: This will abort the entire atomic batch.
                            Err(error) => {
                                return Err(format!("Failed to finalize an accepted execute transaction - {error}"));
                            }
                        }
                        Ok(())
                    }
                    ConfirmedTransaction::RejectedDeploy(_, Transaction::Fee(_, fee), rejected, finalize) => {
                        // Extract the rejected deployment.
                        let Some(deployment) = rejected.deployment() else {
                            // Note: This will abort the entire atomic batch.
                            return Err("Expected rejected deployment".to_string());
                        };
                        // Compute the expected deployment ID.
                        let Ok(expected_deployment_id) = deployment.to_deployment_id() else {
                            // Note: This will abort the entire atomic batch.
                            return Err("Failed to compute the deployment ID for a rejected deployment".to_string());
                        };
                        // Retrieve the candidate deployment ID.
                        let Ok(candidate_deployment_id) = fee.deployment_or_execution_id() else {
                            // Note: This will abort the entire atomic batch.
                            return Err("Failed to retrieve the deployment ID from the fee".to_string());
                        };
                        // Ensure this fee corresponds to the deployment.
                        if candidate_deployment_id != expected_deployment_id {
                            // Note: This will abort the entire atomic batch.
                            return Err("Mismatch in fee for a rejected deploy transaction".to_string());
                        }
                        // Lastly, finalize the fee.
                        match process.finalize_fee(state, store, fee) {
                            // Ensure the finalize operations match the expected.
                            Ok(finalize_operations) => {
                                if finalize != &finalize_operations {
                                    // Note: This will abort the entire atomic batch.
                                    return Err(format!(
                                        "Mismatch in finalize operations for a rejected deploy - (found: {finalize_operations:?}, expected: {finalize:?})"
                                    ));
                                }
                            }
                            // Note: This will abort the entire atomic batch.
                            Err(_e) => {
                                return Err("Failed to finalize the fee in a rejected deploy transaction".to_string());
                            }
                        }
                        Ok(())
                    }
                    ConfirmedTransaction::RejectedExecute(_, Transaction::Fee(_, fee), rejected, finalize) => {
                        // Extract the rejected execution.
                        let Some(execution) = rejected.execution() else {
                            // Note: This will abort the entire atomic batch.
                            return Err("Expected rejected execution".to_string());
                        };
                        // Compute the expected execution ID.
                        let Ok(expected_execution_id) = execution.to_execution_id() else {
                            // Note: This will abort the entire atomic batch.
                            return Err("Failed to compute the execution ID for a rejected execution".to_string());
                        };
                        // Retrieve the candidate execution ID.
                        let Ok(candidate_execution_id) = fee.deployment_or_execution_id() else {
                            // Note: This will abort the entire atomic batch.
                            return Err("Failed to retrieve the execution ID from the fee".to_string());
                        };
                        // Ensure this fee corresponds to the execution.
                        if candidate_execution_id != expected_execution_id {
                            // Note: This will abort the entire atomic batch.
                            return Err("Mismatch in fee for a rejected execute transaction".to_string());
                        }
                        // Lastly, finalize the fee.
                        match process.finalize_fee(state, store, fee) {
                            // Ensure the finalize operations match the expected.
                            Ok(finalize_operations) => {
                                if finalize != &finalize_operations {
                                    // Note: This will abort the entire atomic batch.
                                    return Err(format!(
                                        "Mismatch in finalize operations for a rejected execute - (found: {finalize_operations:?}, expected: {finalize:?})"
                                    ));
                                }
                            }
                            // Note: This will abort the entire atomic batch.
                            Err(_e) => {
                                return Err("Failed to finalize the fee in a rejected execute transaction".to_string());
                            }
                        }
                        Ok(())
                    }
                    // Note: This will abort the entire atomic batch.
                    _ => return Err("Invalid confirmed transaction type".to_string()),
                };
                lap!(timer, "Finalizing transaction {}", transaction.id());

                match outcome {
                    // If the transaction succeeded to finalize, continue to the next transaction.
                    Ok(()) => (),
                    // If the transaction failed to finalize, abort and continue to the next transaction.
                    Err(error) => {
                        eprintln!("Critical bug in finalize: {error}\n\n{transaction}");
                        // Note: This will abort the entire atomic batch.
                        return Err(format!("Failed to finalize on transaction - {error}"));
                    }
                }
            }

            /* Perform the ratifications after finalize. */

            match Self::atomic_post_ratify(store, state, post_ratifications, solutions) {
                // Store the finalize operations from the post-ratify.
                Ok(operations) => ratified_finalize_operations.extend(operations),
                // Note: This will abort the entire atomic batch.
                Err(e) => return Err(format!("Failed to post-ratify - {e}")),
            }

            /* Start the commit process. */

            // Commit all of the stacks to the process.
            if !stacks.is_empty() {
                stacks.into_iter().for_each(|stack| process.add_stack(stack))
            }

            finish!(timer); // <- Note: This timer does **not** include the time to write batch to DB.

            Ok(ratified_finalize_operations)
        })
    }

    /// Performs the pre-ratifications before finalizing transactions.
    #[inline]
    fn atomic_pre_ratify<'a>(
        store: &FinalizeStore<N, C::FinalizeStorage>,
        state: FinalizeGlobalState,
        pre_ratifications: impl Iterator<Item = &'a Ratify<N>>,
    ) -> Result<Vec<FinalizeOperation<N>>> {
        // Construct the program ID.
        let program_id = ProgramID::from_str("credits.aleo")?;
        // Construct the committee mapping name.
        let committee_mapping = Identifier::from_str("committee")?;
        // Construct the bonded mapping name.
        let bonded_mapping = Identifier::from_str("bonded")?;
        // Construct the account mapping name.
        let account_mapping = Identifier::from_str("account")?;
        // Construct the metadata mapping name.
        let metadata_mapping = Identifier::from_str("metadata")?;

        // Initialize a list of finalize operations.
        let mut finalize_operations = Vec::new();

        // Initialize a flag for the genesis ratification.
        let mut is_genesis_ratified = false;

        // Iterate over the ratifications.
        for ratify in pre_ratifications {
            match ratify {
                Ratify::Genesis(committee, public_balances, bonded_balances) => {
                    // Ensure this is the genesis block.
                    ensure!(state.block_height() == 0, "Ratify::Genesis(..) expected a genesis block");
                    // Ensure the genesis committee round is 0.
                    ensure!(
                        committee.starting_round() == 0,
                        "Ratify::Genesis(..) expected a genesis committee round of 0"
                    );
                    // Ensure that the number of members in the committee does not exceed the maximum.
                    ensure!(
                        committee.members().len() <= Committee::<N>::MAX_COMMITTEE_SIZE as usize,
                        "Ratify::Genesis(..) exceeds the maximum number of committee members"
                    );
                    // Ensure that the number of delegators does not exceed the maximum.
                    ensure!(
                        bonded_balances.len().saturating_sub(committee.members().len()) <= MAX_DELEGATORS as usize,
                        "Ratify::Genesis(..) exceeds the maximum number of delegators"
                    );
                    // Ensure genesis has not been ratified yet.
                    ensure!(!is_genesis_ratified, "Ratify::Genesis(..) has already been ratified");

                    // TODO (howardwu): Consider whether to initialize the mappings here.
                    //  Currently, this is breaking for test cases that use VM but do not insert the genesis block.
                    // // Initialize the store for 'credits.aleo'.
                    // let credits = Program::<N>::credits()?;
                    // for mapping in credits.mappings().values() {
                    //     // Ensure that all mappings are initialized.
                    //     if !store.contains_mapping_confirmed(credits.id(), mapping.name())? {
                    //         // Initialize the mappings for 'credits.aleo'.
                    //         finalize_operations.push(store.initialize_mapping(*credits.id(), *mapping.name())?);
                    //     }
                    // }

                    // Calculate the stake per validator using `bonded_balances`.
                    let mut stake_per_validator = IndexMap::with_capacity(committee.members().len());
                    for (address, (validator_address, amount)) in bonded_balances.iter() {
                        // Check that the amount meets the minimum requirement, depending on whether the address is a validator.
                        if *address == *validator_address {
                            ensure!(
                                *amount >= MIN_VALIDATOR_STAKE,
                                "Ratify::Genesis(..) the validator {address} must stake at least {MIN_VALIDATOR_STAKE}",
                            );
                        } else {
                            ensure!(
                                *amount >= MIN_DELEGATOR_STAKE,
                                "Ratify::Genesis(..) the delegator {address} must stake at least {MIN_DELEGATOR_STAKE}",
                            );
                            // If the address is a delegator, check that the corresponding validator is open.
                            ensure!(
                                committee.is_committee_member_open(*validator_address),
                                "Ratify::Genesis(..) the delegator {address} is delegating to a closed validator {validator_address}",
                            );
                        }
                        // Accumulate the staked amount per validator.
                        let total = stake_per_validator.entry(validator_address).or_insert(0u64);
                        *total = total.saturating_add(*amount);
                    }
                    // Ensure the stake per validator matches the committee.
                    ensure!(
                        stake_per_validator.len() == committee.members().len(),
                        "Ratify::Genesis(..) the number of validators in the committee does not match the number of validators in the bonded balances",
                    );

                    // Check that `committee` is consistent with `stake_per_validator`.
                    for (validator_address, amount) in &stake_per_validator {
                        // Retrieve the expected validator stake from the committee.
                        let Some((expected_amount, _)) = committee.members().get(*validator_address) else {
                            bail!(
                                "Ratify::Genesis(..) found a validator in the bonded balances that is not in the committee"
                            )
                        };
                        // Ensure the staked amount matches the committee.
                        ensure!(
                            *expected_amount == *amount,
                            "Ratify::Genesis(..) inconsistent staked amount for validator {validator_address}",
                        );
                    }
                    // Ensure that the total stake matches the sum of the staked amounts.
                    ensure!(
                        committee.total_stake() == stake_per_validator.values().sum::<u64>(),
                        "Ratify::Genesis(..) incorrect total total stake for the committee"
                    );

                    // Construct the next committee map and next bonded map.
                    let (next_committee_map, next_bonded_map) =
                        to_next_commitee_map_and_bonded_map(committee, bonded_balances);

                    // Insert the next committee into storage.
                    store.committee_store().insert(state.block_height(), *(committee.clone()))?;
                    // Store the finalize operations for updating the committee and bonded mapping.
                    finalize_operations.extend(&[
                        // Replace the committee mapping in storage.
                        store.replace_mapping(program_id, committee_mapping, next_committee_map)?,
                        // Replace the bonded mapping in storage.
                        store.replace_mapping(program_id, bonded_mapping, next_bonded_map)?,
                    ]);

                    // Update the number of validators.
                    finalize_operations.extend(&[
                        // Update the number of validators in the metadata mapping.
                        store.update_key_value(
                            program_id,
                            metadata_mapping,
                            Plaintext::from_str("aleo1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq3ljyzc")?,
                            Value::from_str(&format!("{}u32", committee.num_members()))?,
                        )?,
                    ]);

                    // Update the number of delegators.
                    finalize_operations.extend(&[
                        // Update the number of delegators in the metadata mapping.
                        store.update_key_value(
                            program_id,
                            metadata_mapping,
                            Plaintext::from_str("aleo1qgqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqanmpl0")?,
                            Value::from_str(&format!(
                                "{}u32",
                                bonded_balances.len().saturating_sub(committee.num_members())
                            ))?,
                        )?,
                    ]);

                    // Map the public balances into the appropriate format.
                    let public_balances = public_balances
                        .iter()
                        .map(|(address, amount)| {
                            (Plaintext::from(Literal::Address(*address)), Value::from(Literal::U64(U64::new(*amount))))
                        })
                        .collect::<Vec<_>>();

                    // Update the public balances.
                    finalize_operations.extend(&[
                        // Update the public balances in storage.
                        store.replace_mapping(program_id, account_mapping, public_balances)?,
                    ]);

                    // Set the genesis ratification flag.
                    is_genesis_ratified = true;
                }
                Ratify::BlockReward(..) | Ratify::PuzzleReward(..) => continue,
            }
        }

        // Return the finalize operations.
        Ok(finalize_operations)
    }

    /// Performs the post-ratifications after finalizing transactions.
    #[inline]
    fn atomic_post_ratify<'a>(
        store: &FinalizeStore<N, C::FinalizeStorage>,
        state: FinalizeGlobalState,
        post_ratifications: impl Iterator<Item = &'a Ratify<N>>,
        solutions: &Solutions<N>,
    ) -> Result<Vec<FinalizeOperation<N>>> {
        // Construct the program ID.
        let program_id = ProgramID::from_str("credits.aleo")?;
        // Construct the committee mapping name.
        let committee_mapping = Identifier::from_str("committee")?;
        // Construct the bonded mapping name.
        let bonded_mapping = Identifier::from_str("bonded")?;
        // Construct the account mapping name.
        let account_mapping = Identifier::from_str("account")?;

        // Initialize a list of finalize operations.
        let mut finalize_operations = Vec::new();

        // Initialize a flag for the block reward ratification.
        let mut is_block_reward_ratified = false;
        // Initialize a flag for the puzzle reward ratification.
        let mut is_puzzle_reward_ratified = false;

        // Iterate over the ratifications.
        for ratify in post_ratifications {
            match ratify {
                Ratify::Genesis(..) => continue,
                Ratify::BlockReward(block_reward) => {
                    // Ensure the block reward has not been ratified yet.
                    ensure!(!is_block_reward_ratified, "Ratify::BlockReward(..) has already been ratified");

                    // Retrieve the committee mapping from storage.
                    let current_committee_map = store.get_mapping_speculative(program_id, committee_mapping)?;
                    // Convert the committee mapping into a committee.
                    let current_committee = committee_map_into_committee(state.block_round(), current_committee_map)?;
                    // Retrieve the bonded mapping from storage.
                    let current_bonded_map = store.get_mapping_speculative(program_id, bonded_mapping)?;
                    // Convert the bonded map into stakers.
                    let current_stakers = bonded_map_into_stakers(current_bonded_map)?;

                    // Ensure the committee matches the bonded mapping.
                    ensure_stakers_matches(&current_committee, &current_stakers)?;

                    // Compute the updated stakers, using the committee and block reward.
                    let next_stakers = staking_rewards(&current_stakers, &current_committee, *block_reward);
                    // Compute the updated committee, using the stakers.
                    let next_committee = to_next_committee(&current_committee, state.block_round(), &next_stakers)?;

                    // Construct the next committee map and next bonded map.
                    let (next_committee_map, next_bonded_map) =
                        to_next_commitee_map_and_bonded_map(&next_committee, &next_stakers);

                    // Insert the next committee into storage.
                    store.committee_store().insert(state.block_height(), next_committee)?;
                    // Store the finalize operations for updating the committee and bonded mapping.
                    finalize_operations.extend(&[
                        // Replace the committee mapping in storage.
                        store.replace_mapping(program_id, committee_mapping, next_committee_map)?,
                        // Replace the bonded mapping in storage.
                        store.replace_mapping(program_id, bonded_mapping, next_bonded_map)?,
                    ]);

                    // Set the block reward ratification flag.
                    is_block_reward_ratified = true;
                }
                Ratify::PuzzleReward(puzzle_reward) => {
                    // Ensure the puzzle reward has not been ratified yet.
                    ensure!(!is_puzzle_reward_ratified, "Ratify::PuzzleReward(..) has already been ratified");

                    // If the puzzle reward is zero, skip.
                    if *puzzle_reward == 0 {
                        continue;
                    }
                    // Retrieve the solutions.
                    let Some(solutions) = solutions.deref() else {
                        continue;
                    };
                    // Compute the proof targets, with the corresponding addresses.
                    let proof_targets =
                        solutions.values().map(|s| Ok((s.address(), s.to_target()?))).collect::<Result<Vec<_>>>()?;
                    // Calculate the proving rewards.
                    let proving_rewards = proving_rewards(proof_targets, *puzzle_reward);
                    // Iterate over the proving rewards.
                    for (address, amount) in proving_rewards {
                        // Construct the key.
                        let key = Plaintext::from(Literal::Address(address));
                        // Retrieve the current public balance.
                        let value = store.get_value_speculative(program_id, account_mapping, &key)?;
                        // Compute the next public balance.
                        let next_value = Value::from(Literal::U64(U64::new(match value {
                            Some(Value::Plaintext(Plaintext::Literal(Literal::U64(value), _))) => {
                                (*value).saturating_add(amount)
                            }
                            None => amount,
                            v => bail!("Critical bug in post-ratify puzzle reward- Invalid amount ({v:?})"),
                        })));
                        // Update the public balance in finalize storage.
                        let operation = store.update_key_value(program_id, account_mapping, key, next_value)?;
                        finalize_operations.push(operation);
                    }

                    // Set the puzzle reward ratification flag.
                    is_puzzle_reward_ratified = true;
                }
            }
        }

        // Return the finalize operations.
        Ok(finalize_operations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{test_helpers, test_helpers::sample_finalize_state};
    use console::{
        account::{Address, PrivateKey, ViewKey},
        program::{Ciphertext, Entry, Record},
        types::Field,
    };
    use ledger_block::{Block, Header, Metadata, Transaction, Transition};
    use ledger_committee::{MAX_DELEGATORS, MIN_VALIDATOR_STAKE};
    use ledger_store::helpers::memory::ConsensusMemory;
    use synthesizer_program::Program;

    use rand::distributions::DistString;

    type CurrentNetwork = test_helpers::CurrentNetwork;

    /// Sample a new program and deploy it to the VM. Returns the program name.
    fn new_program_deployment<R: Rng + CryptoRng>(
        vm: &VM<CurrentNetwork, ConsensusMemory<CurrentNetwork>>,
        private_key: &PrivateKey<CurrentNetwork>,
        previous_block: &Block<CurrentNetwork>,
        unspent_records: &mut Vec<Record<CurrentNetwork, Ciphertext<CurrentNetwork>>>,
        rng: &mut R,
    ) -> Result<(String, Block<CurrentNetwork>)> {
        let program_name = format!("a{}.aleo", Alphanumeric.sample_string(rng, 8).to_lowercase());

        let program = Program::<CurrentNetwork>::from_str(&format!(
            "
program {program_name};

mapping account:
    // The token owner.
    key as address.public;
    // The token amount.
    value as u64.public;

function mint_public:
    input r0 as address.public;
    input r1 as u64.public;
    async mint_public r0 r1 into r2;
    output r2 as {program_name}/mint_public.future;

finalize mint_public:
    input r0 as address.public;
    input r1 as u64.public;

    get.or_use account[r0] 0u64 into r2;
    add r2 r1 into r3;
    set r3 into account[r0];

function transfer_public:
    input r0 as address.public;
    input r1 as u64.public;
    async transfer_public self.caller r0 r1 into r2;
    output r2 as {program_name}/transfer_public.future;

finalize transfer_public:
    input r0 as address.public;
    input r1 as address.public;
    input r2 as u64.public;

    get.or_use account[r0] 0u64 into r3;
    get.or_use account[r1] 0u64 into r4;

    sub r3 r2 into r5;
    add r4 r2 into r6;

    set r5 into account[r0];
    set r6 into account[r1];"
        ))?;

        // Prepare the additional fee.
        let view_key = ViewKey::<CurrentNetwork>::try_from(private_key)?;
        let credits = Some(unspent_records.pop().unwrap().decrypt(&view_key)?);

        // Deploy.
        let transaction = vm.deploy(private_key, &program, credits, 10, None, rng)?;

        // Construct the new block.
        let next_block = sample_next_block(vm, private_key, &[transaction], previous_block, unspent_records, rng)?;

        Ok((program_name, next_block))
    }

    /// Construct a new block based on the given transactions.
    fn sample_next_block<R: Rng + CryptoRng>(
        vm: &VM<CurrentNetwork, ConsensusMemory<CurrentNetwork>>,
        private_key: &PrivateKey<CurrentNetwork>,
        transactions: &[Transaction<CurrentNetwork>],
        previous_block: &Block<CurrentNetwork>,
        unspent_records: &mut Vec<Record<CurrentNetwork, Ciphertext<CurrentNetwork>>>,
        rng: &mut R,
    ) -> Result<Block<CurrentNetwork>> {
        // Speculate on the candidate ratifications, solutions, and transactions.
        let (ratifications, transactions, aborted_transaction_ids, ratified_finalize_operations) = vm.speculate(
            sample_finalize_state(previous_block.height() + 1),
            None,
            vec![],
            &None.into(),
            transactions.iter(),
        )?;

        // Construct the metadata associated with the block.
        let metadata = Metadata::new(
            CurrentNetwork::ID,
            previous_block.round() + 1,
            previous_block.height() + 1,
            0,
            0,
            CurrentNetwork::GENESIS_COINBASE_TARGET,
            CurrentNetwork::GENESIS_PROOF_TARGET,
            previous_block.last_coinbase_target(),
            previous_block.last_coinbase_timestamp(),
            CurrentNetwork::GENESIS_TIMESTAMP + 1,
        )?;

        // Construct the new block header.
        let header = Header::from(
            vm.block_store().current_state_root(),
            transactions.to_transactions_root().unwrap(),
            transactions.to_finalize_root(ratified_finalize_operations).unwrap(),
            ratifications.to_ratifications_root().unwrap(),
            Field::zero(),
            Field::zero(),
            metadata,
        )?;

        let block = Block::new_beacon(
            private_key,
            previous_block.hash(),
            header,
            ratifications,
            None.into(),
            vec![],
            transactions,
            aborted_transaction_ids,
            rng,
        )?;

        // Track the new records.
        let new_records = block
            .transitions()
            .cloned()
            .flat_map(Transition::into_records)
            .map(|(_, record)| record)
            .collect::<Vec<_>>();
        unspent_records.extend(new_records);

        Ok(block)
    }

    /// Generate split transactions for the unspent records.
    fn generate_splits<R: Rng + CryptoRng>(
        vm: &VM<CurrentNetwork, ConsensusMemory<CurrentNetwork>>,
        private_key: &PrivateKey<CurrentNetwork>,
        previous_block: &Block<CurrentNetwork>,
        unspent_records: &mut Vec<Record<CurrentNetwork, Ciphertext<CurrentNetwork>>>,
        rng: &mut R,
    ) -> Result<Block<CurrentNetwork>> {
        // Prepare the additional fee.
        let view_key = ViewKey::<CurrentNetwork>::try_from(private_key)?;

        // Generate split transactions.
        let mut transactions = Vec::new();
        while !unspent_records.is_empty() {
            let record = unspent_records.pop().unwrap().decrypt(&view_key)?;

            // Fetch the record balance and divide it in half.
            let split_balance = match record.find(&[Identifier::from_str("microcredits")?]) {
                Ok(Entry::Private(Plaintext::Literal(Literal::U64(amount), _))) => *amount / 2,
                _ => bail!("fee record does not contain a microcredits entry"),
            };

            // Prepare the inputs.
            let inputs = [
                Value::<CurrentNetwork>::Record(record),
                Value::<CurrentNetwork>::from_str(&format!("{split_balance}u64")).unwrap(),
            ]
            .into_iter();

            // Execute.
            let transaction = vm.execute(private_key, ("credits.aleo", "split"), inputs, None, 0, None, rng).unwrap();

            transactions.push(transaction);
        }

        // Construct the new block.
        sample_next_block(vm, private_key, &transactions, previous_block, unspent_records, rng)
    }

    /// Create an execution transaction.
    fn create_execution(
        vm: &VM<CurrentNetwork, ConsensusMemory<CurrentNetwork>>,
        caller_private_key: PrivateKey<CurrentNetwork>,
        program_id: &str,
        function_name: &str,
        inputs: Vec<Value<CurrentNetwork>>,
        unspent_records: &mut Vec<Record<CurrentNetwork, Ciphertext<CurrentNetwork>>>,
        rng: &mut TestRng,
    ) -> Transaction<CurrentNetwork> {
        assert!(vm.contains_program(&ProgramID::from_str(program_id).unwrap()));

        // Prepare the additional fee.
        let view_key = ViewKey::<CurrentNetwork>::try_from(caller_private_key).unwrap();
        let credits = Some(unspent_records.pop().unwrap().decrypt(&view_key).unwrap());

        // Execute.
        let transaction = vm
            .execute(&caller_private_key, (program_id, function_name), inputs.into_iter(), credits, 1, None, rng)
            .unwrap();
        // Verify.
        vm.check_transaction(&transaction, None, rng).unwrap();

        // Return the transaction.
        transaction
    }

    /// Sample a public mint transaction.
    fn sample_mint_public(
        vm: &VM<CurrentNetwork, ConsensusMemory<CurrentNetwork>>,
        caller_private_key: PrivateKey<CurrentNetwork>,
        program_id: &str,
        recipient: Address<CurrentNetwork>,
        amount: u64,
        unspent_records: &mut Vec<Record<CurrentNetwork, Ciphertext<CurrentNetwork>>>,
        rng: &mut TestRng,
    ) -> Transaction<CurrentNetwork> {
        let inputs = vec![
            Value::<CurrentNetwork>::from_str(&recipient.to_string()).unwrap(),
            Value::<CurrentNetwork>::from_str(&format!("{amount}u64")).unwrap(),
        ];

        create_execution(vm, caller_private_key, program_id, "mint_public", inputs, unspent_records, rng)
    }

    /// Sample a public transfer transaction.
    fn sample_transfer_public(
        vm: &VM<CurrentNetwork, ConsensusMemory<CurrentNetwork>>,
        caller_private_key: PrivateKey<CurrentNetwork>,
        program_id: &str,
        recipient: Address<CurrentNetwork>,
        amount: u64,
        unspent_records: &mut Vec<Record<CurrentNetwork, Ciphertext<CurrentNetwork>>>,
        rng: &mut TestRng,
    ) -> Transaction<CurrentNetwork> {
        let inputs = vec![
            Value::<CurrentNetwork>::from_str(&recipient.to_string()).unwrap(),
            Value::<CurrentNetwork>::from_str(&format!("{amount}u64")).unwrap(),
        ];

        create_execution(vm, caller_private_key, program_id, "transfer_public", inputs, unspent_records, rng)
    }

    /// A helper method to construct the rejected transaction format for `atomic_finalize`.
    fn reject(
        index: u32,
        transaction: &Transaction<CurrentNetwork>,
        finalize: &[FinalizeOperation<CurrentNetwork>],
    ) -> ConfirmedTransaction<CurrentNetwork> {
        match transaction {
            Transaction::Execute(_, execution, fee) => ConfirmedTransaction::RejectedExecute(
                index,
                Transaction::from_fee(fee.clone().unwrap()).unwrap(),
                Rejected::new_execution(execution.clone()),
                finalize.to_vec(),
            ),
            _ => panic!("only reject execution transactions"),
        }
    }

    #[test]
    fn test_finalize_duplicate_deployment() {
        let rng = &mut TestRng::default();

        let vm = crate::vm::test_helpers::sample_vm();

        // Fetch a deployment transaction.
        let deployment_transaction = crate::vm::test_helpers::sample_deployment_transaction(rng);
        let deployment_transaction_id = deployment_transaction.id();

        // Construct the program name.
        let program_id = ProgramID::from_str("testing.aleo").unwrap();

        // Prepare the confirmed transactions.
        let (ratifications, confirmed_transactions, aborted_transaction_ids, _) = vm
            .speculate(sample_finalize_state(1), None, vec![], &None.into(), [deployment_transaction.clone()].iter())
            .unwrap();
        assert_eq!(confirmed_transactions.len(), 1);
        assert!(aborted_transaction_ids.is_empty());

        // Ensure the VM does not contain this program.
        assert!(!vm.contains_program(&program_id));

        // Finalize the transaction.
        assert!(vm.finalize(sample_finalize_state(1), &ratifications, &None.into(), &confirmed_transactions).is_ok());

        // Ensure the VM contains this program.
        assert!(vm.contains_program(&program_id));

        // Ensure the VM can't redeploy the same transaction.
        assert!(vm.finalize(sample_finalize_state(1), &ratifications, &None.into(), &confirmed_transactions).is_err());

        // Ensure the VM contains this program.
        assert!(vm.contains_program(&program_id));

        // Ensure the dry run of the redeployment will cause a reject transaction to be created.
        let (_, candidate_transactions, aborted_transaction_ids, _) = vm
            .atomic_speculate(sample_finalize_state(1), None, vec![], &None.into(), [deployment_transaction].iter())
            .unwrap();
        assert_eq!(candidate_transactions.len(), 1);
        assert!(matches!(candidate_transactions[0], ConfirmedTransaction::RejectedDeploy(..)));
        assert!(aborted_transaction_ids.is_empty());

        // Check that the unconfirmed transaction ID of the rejected deployment is correct.
        assert_eq!(candidate_transactions[0].to_unconfirmed_transaction_id().unwrap(), deployment_transaction_id);
    }

    #[test]
    fn test_atomic_finalize_many() {
        let rng = &mut TestRng::default();

        // Sample a private key and address for the caller.
        let caller_private_key = test_helpers::sample_genesis_private_key(rng);
        let caller_address = Address::try_from(&caller_private_key).unwrap();

        // Sample a private key and address for the recipient.
        let recipient_private_key = PrivateKey::new(rng).unwrap();
        let recipient_address = Address::try_from(&recipient_private_key).unwrap();

        // Initialize the vm.
        let vm = test_helpers::sample_vm_with_genesis_block(rng);

        // Deploy a new program.
        let genesis =
            vm.block_store().get_block(&vm.block_store().get_block_hash(0).unwrap().unwrap()).unwrap().unwrap();

        // Get the unspent records.
        let mut unspent_records = genesis
            .transitions()
            .cloned()
            .flat_map(Transition::into_records)
            .map(|(_, record)| record)
            .collect::<Vec<_>>();

        // Construct the deployment block.
        let (program_id, deployment_block) =
            new_program_deployment(&vm, &caller_private_key, &genesis, &mut unspent_records, rng).unwrap();

        // Add the deployment block to the VM.
        vm.add_next_block(&deployment_block).unwrap();

        // Generate more records to use for the next block.
        let splits_block =
            generate_splits(&vm, &caller_private_key, &deployment_block, &mut unspent_records, rng).unwrap();

        // Add the splits block to the VM.
        vm.add_next_block(&splits_block).unwrap();

        // Construct the initial mint.
        let initial_mint =
            sample_mint_public(&vm, caller_private_key, &program_id, caller_address, 20, &mut unspent_records, rng);
        let initial_mint_block =
            sample_next_block(&vm, &caller_private_key, &[initial_mint], &splits_block, &mut unspent_records, rng)
                .unwrap();

        // Add the block to the vm.
        vm.add_next_block(&initial_mint_block).unwrap();

        // Construct a mint and a transfer.
        let mint_10 =
            sample_mint_public(&vm, caller_private_key, &program_id, caller_address, 10, &mut unspent_records, rng);
        let mint_20 =
            sample_mint_public(&vm, caller_private_key, &program_id, caller_address, 20, &mut unspent_records, rng);
        let transfer_10 = sample_transfer_public(
            &vm,
            caller_private_key,
            &program_id,
            recipient_address,
            10,
            &mut unspent_records,
            rng,
        );
        let transfer_20 = sample_transfer_public(
            &vm,
            caller_private_key,
            &program_id,
            recipient_address,
            20,
            &mut unspent_records,
            rng,
        );
        let transfer_30 = sample_transfer_public(
            &vm,
            caller_private_key,
            &program_id,
            recipient_address,
            30,
            &mut unspent_records,
            rng,
        );

        // TODO (raychu86): Confirm that the finalize_operations here are correct.

        // Starting Balance = 20
        // Mint_10 -> Balance = 20 + 10  = 30
        // Transfer_10 -> Balance = 30 - 10 = 20
        // Transfer_20 -> Balance = 20 - 20 = 0
        {
            let transactions = [mint_10.clone(), transfer_10.clone(), transfer_20.clone()];
            let (_, confirmed_transactions, aborted_transaction_ids, _) =
                vm.atomic_speculate(sample_finalize_state(1), None, vec![], &None.into(), transactions.iter()).unwrap();

            // Assert that all the transactions are accepted.
            assert_eq!(confirmed_transactions.len(), 3);
            confirmed_transactions.iter().for_each(|confirmed_tx| assert!(confirmed_tx.is_accepted()));
            assert!(aborted_transaction_ids.is_empty());

            assert_eq!(confirmed_transactions[0].transaction(), &mint_10);
            assert_eq!(confirmed_transactions[1].transaction(), &transfer_10);
            assert_eq!(confirmed_transactions[2].transaction(), &transfer_20);
        }

        // Starting Balance = 20
        // Transfer_20 -> Balance = 20 - 20 = 0
        // Mint_10 -> Balance = 0 + 10 = 10
        // Mint_20 -> Balance = 10 + 20 = 30
        // Transfer_30 -> Balance = 30 - 30 = 0
        {
            let transactions = [transfer_20.clone(), mint_10.clone(), mint_20.clone(), transfer_30.clone()];
            let (_, confirmed_transactions, aborted_transaction_ids, _) =
                vm.atomic_speculate(sample_finalize_state(1), None, vec![], &None.into(), transactions.iter()).unwrap();

            // Assert that all the transactions are accepted.
            assert_eq!(confirmed_transactions.len(), 4);
            confirmed_transactions.iter().for_each(|confirmed_tx| assert!(confirmed_tx.is_accepted()));
            assert!(aborted_transaction_ids.is_empty());

            // Ensure that the transactions are in the correct order.
            assert_eq!(confirmed_transactions[0].transaction(), &transfer_20);
            assert_eq!(confirmed_transactions[1].transaction(), &mint_10);
            assert_eq!(confirmed_transactions[2].transaction(), &mint_20);
            assert_eq!(confirmed_transactions[3].transaction(), &transfer_30);
        }

        // Starting Balance = 20
        // Transfer_20 -> Balance = 20 - 20 = 0
        // Transfer_10 -> Balance = 0 - 10 = -10 (should be rejected)
        {
            let transactions = [transfer_20.clone(), transfer_10.clone()];
            let (_, confirmed_transactions, aborted_transaction_ids, _) =
                vm.atomic_speculate(sample_finalize_state(1), None, vec![], &None.into(), transactions.iter()).unwrap();

            // Assert that the accepted and rejected transactions are correct.
            assert_eq!(confirmed_transactions.len(), 2);
            assert!(aborted_transaction_ids.is_empty());

            assert!(confirmed_transactions[0].is_accepted());
            assert!(confirmed_transactions[1].is_rejected());

            assert_eq!(confirmed_transactions[0].transaction(), &transfer_20);
            assert_eq!(
                confirmed_transactions[1],
                reject(1, &transfer_10, confirmed_transactions[1].finalize_operations())
            );
        }

        // Starting Balance = 20
        // Mint_20 -> Balance = 20 + 20
        // Transfer_30 -> Balance = 40 - 30 = 10
        // Transfer_20 -> Balance = 10 - 20 = -10 (should be rejected)
        // Transfer_10 -> Balance = 10 - 10 = 0
        {
            let transactions = [mint_20.clone(), transfer_30.clone(), transfer_20.clone(), transfer_10.clone()];
            let (_, confirmed_transactions, aborted_transaction_ids, _) =
                vm.atomic_speculate(sample_finalize_state(1), None, vec![], &None.into(), transactions.iter()).unwrap();

            // Assert that the accepted and rejected transactions are correct.
            assert_eq!(confirmed_transactions.len(), 4);
            assert!(aborted_transaction_ids.is_empty());

            assert!(confirmed_transactions[0].is_accepted());
            assert!(confirmed_transactions[1].is_accepted());
            assert!(confirmed_transactions[2].is_rejected());
            assert!(confirmed_transactions[3].is_accepted());

            assert_eq!(confirmed_transactions[0].transaction(), &mint_20);
            assert_eq!(confirmed_transactions[1].transaction(), &transfer_30);
            assert_eq!(
                confirmed_transactions[2],
                reject(2, &transfer_20, confirmed_transactions[2].finalize_operations())
            );
            assert_eq!(confirmed_transactions[3].transaction(), &transfer_10);
        }
    }

    #[test]
    fn test_finalize_catch_halt() {
        let rng = &mut TestRng::default();

        // Sample a private key, view key, and address for the caller.
        let caller_private_key = test_helpers::sample_genesis_private_key(rng);
        let caller_view_key = ViewKey::try_from(&caller_private_key).unwrap();

        for finalize_logic in &[
            "finalize ped_hash:
    input r0 as u128.public;
    hash.ped64 r0 into r1 as field;
    set r1 into hashes[r0];",
            "finalize ped_hash:
    input r0 as u128.public;
    div r0 0u128 into r1;",
        ] {
            // Initialize the vm.
            let vm = test_helpers::sample_vm_with_genesis_block(rng);

            // Deploy a new program.
            let genesis =
                vm.block_store().get_block(&vm.block_store().get_block_hash(0).unwrap().unwrap()).unwrap().unwrap();

            // Get the unspent records.
            let mut unspent_records = genesis
                .transitions()
                .cloned()
                .flat_map(Transition::into_records)
                .map(|(_, record)| record)
                .collect::<Vec<_>>();

            // Create a program that will always cause a E::halt in the finalize execution.
            let program_id = "testing.aleo";
            let program = Program::<CurrentNetwork>::from_str(&format!(
                "
program {program_id};

mapping hashes:
    key as u128.public;
    value as field.public;

function ped_hash:
    input r0 as u128.public;
    // hash.ped64 r0 into r1 as field; // <--- This will cause a E::halt.
    async ped_hash r0 into r1;
    output r1 as {program_id}/ped_hash.future;

{finalize_logic}"
            ))
            .unwrap();

            let credits = Some(unspent_records.pop().unwrap().decrypt(&caller_view_key).unwrap());

            // Deploy the program.
            let deployment_transaction = vm.deploy(&caller_private_key, &program, credits, 10, None, rng).unwrap();

            // Construct the deployment block.
            let deployment_block = sample_next_block(
                &vm,
                &caller_private_key,
                &[deployment_transaction],
                &genesis,
                &mut unspent_records,
                rng,
            )
            .unwrap();

            // Add the deployment block to the VM.
            vm.add_next_block(&deployment_block).unwrap();

            // Construct a transaction that will cause a E::halt in the finalize execution.
            let inputs = vec![Value::<CurrentNetwork>::from_str("1u128").unwrap()];
            let transaction =
                create_execution(&vm, caller_private_key, program_id, "ped_hash", inputs, &mut unspent_records, rng);

            // Speculatively execute the transaction. Ensure that this call does not panic and returns a rejected transaction.
            let (_, confirmed_transactions, aborted_transaction_ids, _) = vm
                .speculate(sample_finalize_state(1), None, vec![], &None.into(), [transaction.clone()].iter())
                .unwrap();
            assert!(aborted_transaction_ids.is_empty());

            // Ensure that the transaction is rejected.
            assert_eq!(confirmed_transactions.len(), 1);
            assert!(transaction.is_execute());
            if let Transaction::Execute(_, execution, fee) = transaction {
                let fee_transaction = Transaction::from_fee(fee.unwrap()).unwrap();
                let expected_confirmed_transaction = ConfirmedTransaction::RejectedExecute(
                    0,
                    fee_transaction,
                    Rejected::new_execution(execution),
                    vec![],
                );

                let confirmed_transaction = confirmed_transactions.iter().next().unwrap();
                assert_eq!(confirmed_transaction, &expected_confirmed_transaction);
            }
        }
    }

    #[test]
    fn test_rejected_transaction_should_not_update_storage() {
        let rng = &mut TestRng::default();

        // Sample a private key.
        let private_key = test_helpers::sample_genesis_private_key(rng);
        let address = Address::try_from(&private_key).unwrap();

        // Initialize the vm.
        let vm = test_helpers::sample_vm_with_genesis_block(rng);

        // Deploy a new program.
        let genesis =
            vm.block_store().get_block(&vm.block_store().get_block_hash(0).unwrap().unwrap()).unwrap().unwrap();

        // Get the unspent records.
        let mut unspent_records = genesis
            .transitions()
            .cloned()
            .flat_map(Transition::into_records)
            .map(|(_, record)| record)
            .collect::<Vec<_>>();

        // Generate more records to use for the next block.
        let splits_block = generate_splits(&vm, &private_key, &genesis, &mut unspent_records, rng).unwrap();

        // Add the splits block to the VM.
        vm.add_next_block(&splits_block).unwrap();

        // Construct the deployment block.
        let deployment_block = {
            let program = Program::<CurrentNetwork>::from_str(
                "
program testing.aleo;

mapping entries:
    key as address.public;
    value as u8.public;

function compute:
    input r0 as u8.public;
    async compute self.caller r0 into r1;
    output r1 as testing.aleo/compute.future;

finalize compute:
    input r0 as address.public;
    input r1 as u8.public;
    get.or_use entries[r0] r1 into r2;
    add r1 r2 into r3;
    set r3 into entries[r0];
    get entries[r0] into r4;
    add r4 r1 into r5;
    set r5 into entries[r0];
",
            )
            .unwrap();

            // Prepare the additional fee.
            let view_key = ViewKey::<CurrentNetwork>::try_from(private_key).unwrap();
            let credits = Some(unspent_records.pop().unwrap().decrypt(&view_key).unwrap());

            // Deploy.
            let transaction = vm.deploy(&private_key, &program, credits, 10, None, rng).unwrap();

            // Construct the new block.
            sample_next_block(&vm, &private_key, &[transaction], &splits_block, &mut unspent_records, rng).unwrap()
        };

        // Add the deployment block to the VM.
        vm.add_next_block(&deployment_block).unwrap();

        // Generate more records to use for the next block.
        let splits_block = generate_splits(&vm, &private_key, &deployment_block, &mut unspent_records, rng).unwrap();

        // Add the splits block to the VM.
        vm.add_next_block(&splits_block).unwrap();

        // Create an execution transaction, that will be rejected.
        let r0 = Value::<CurrentNetwork>::from_str("100u8").unwrap();
        let first = create_execution(&vm, private_key, "testing.aleo", "compute", vec![r0], &mut unspent_records, rng);

        // Construct the next block.
        let next_block =
            sample_next_block(&vm, &private_key, &[first], &splits_block, &mut unspent_records, rng).unwrap();

        // Check that the transaction was rejected.
        assert!(next_block.transactions().iter().next().unwrap().is_rejected());

        // Add the next block to the VM.
        vm.add_next_block(&next_block).unwrap();

        // Check that the storage was not updated.
        let program_id = ProgramID::from_str("testing.aleo").unwrap();
        let mapping_name = Identifier::from_str("entries").unwrap();
        assert!(
            !vm.finalize_store()
                .contains_key_confirmed(program_id, mapping_name, &Plaintext::from(Literal::Address(address)))
                .unwrap()
        );

        // Create an execution transaction, that will be rejected.
        let r0 = Value::<CurrentNetwork>::from_str("100u8").unwrap();
        let first = create_execution(&vm, private_key, "testing.aleo", "compute", vec![r0], &mut unspent_records, rng);

        // Create an execution transaction, that will be accepted.
        let r0 = Value::<CurrentNetwork>::from_str("1u8").unwrap();
        let second = create_execution(&vm, private_key, "testing.aleo", "compute", vec![r0], &mut unspent_records, rng);

        // Construct the next block.
        let next_block =
            sample_next_block(&vm, &private_key, &[first, second], &next_block, &mut unspent_records, rng).unwrap();

        // Check that the first transaction was rejected.
        assert!(next_block.transactions().iter().next().unwrap().is_rejected());

        // Add the next block to the VM.
        vm.add_next_block(&next_block).unwrap();

        // Check that the storage was updated correctly.
        let value = vm
            .finalize_store()
            .get_value_speculative(program_id, mapping_name, &Plaintext::from(Literal::Address(address)))
            .unwrap()
            .unwrap();
        let expected = Value::<CurrentNetwork>::from_str("3u8").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn test_excess_transactions_should_be_aborted() {
        let rng = &mut TestRng::default();

        // Sample a private key.
        let caller_private_key = test_helpers::sample_genesis_private_key(rng);
        let caller_address = Address::try_from(&caller_private_key).unwrap();

        // Initialize the vm.
        let vm = test_helpers::sample_vm_with_genesis_block(rng);

        // Deploy a new program.
        let genesis =
            vm.block_store().get_block(&vm.block_store().get_block_hash(0).unwrap().unwrap()).unwrap().unwrap();

        // Get the unspent records.
        let mut unspent_records = genesis
            .transitions()
            .cloned()
            .flat_map(Transition::into_records)
            .map(|(_, record)| record)
            .collect::<Vec<_>>();

        // Construct the deployment block.
        let (program_id, deployment_block) =
            new_program_deployment(&vm, &caller_private_key, &genesis, &mut unspent_records, rng).unwrap();

        // Add the deployment block to the VM.
        vm.add_next_block(&deployment_block).unwrap();

        // Generate more records to use for the next block.
        let splits_block =
            generate_splits(&vm, &caller_private_key, &deployment_block, &mut unspent_records, rng).unwrap();

        // Add the splits block to the VM.
        vm.add_next_block(&splits_block).unwrap();

        // Generate more records to use for the next block.
        let splits_block = generate_splits(&vm, &caller_private_key, &splits_block, &mut unspent_records, rng).unwrap();

        // Add the splits block to the VM.
        vm.add_next_block(&splits_block).unwrap();

        // Generate the transactions.
        let mut transactions = Vec::new();
        let mut excess_transaction_ids = Vec::new();

        for _ in 0..VM::<CurrentNetwork, ConsensusMemory<_>>::MAXIMUM_CONFIRMED_TRANSACTIONS + 1 {
            let transaction =
                sample_mint_public(&vm, caller_private_key, &program_id, caller_address, 10, &mut unspent_records, rng);
            // Abort the transaction if the block is full.
            if transactions.len() >= VM::<CurrentNetwork, ConsensusMemory<_>>::MAXIMUM_CONFIRMED_TRANSACTIONS {
                excess_transaction_ids.push(transaction.id());
            }

            transactions.push(transaction);
        }

        // Construct the next block.
        let next_block =
            sample_next_block(&vm, &caller_private_key, &transactions, &splits_block, &mut unspent_records, rng)
                .unwrap();

        // Ensure that the excess transactions were aborted.
        assert_eq!(next_block.aborted_transaction_ids(), &excess_transaction_ids);
        assert_eq!(
            next_block.transactions().len(),
            VM::<CurrentNetwork, ConsensusMemory<_>>::MAXIMUM_CONFIRMED_TRANSACTIONS
        );
    }

    #[test]
    fn test_ratify_genesis_greater_than_max_committee_size() {
        // Initialize an RNG.
        let rng = &mut TestRng::default();

        // Initialize the VM.
        let vm =
            VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None).unwrap()).unwrap();

        // Construct the validators, greater than the maximum committee size.
        let validators = (0..(Committee::<CurrentNetwork>::MAX_COMMITTEE_SIZE + 1))
            .map(|_| {
                let private_key = PrivateKey::<CurrentNetwork>::new(rng).unwrap();
                let amount = MIN_VALIDATOR_STAKE;
                let is_open = true;
                (private_key, (amount, is_open))
            })
            .collect::<IndexMap<_, _>>();

        // Construct the committee.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, _)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            committee_map.insert(address, (*amount, true));
        }

        // Attempt to construct a `Committee` with more than the maximum committee size.
        let result = Committee::new_genesis(committee_map);
        assert!(result.is_err());

        // Reset the validators.
        let validators = (0..Committee::<CurrentNetwork>::MAX_COMMITTEE_SIZE)
            .map(|_| {
                let private_key = PrivateKey::new(rng).unwrap();
                let amount = MIN_VALIDATOR_STAKE;
                let is_open = true;
                (private_key, (amount, is_open))
            })
            .collect::<IndexMap<_, _>>();

        // Track the allocated amount.
        let mut allocated_amount = 0;

        // Construct the committee.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, _)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            committee_map.insert(address, (*amount, true));
            allocated_amount += *amount;
        }

        // Construct the public balances, allocating the remaining supply.
        let mut public_balances = IndexMap::new();
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        for (private_key, _) in &validators {
            let address = Address::try_from(private_key).unwrap();
            let amount = remaining_supply / validators.len() as u64;
            allocated_amount += amount;
            public_balances.insert(address, amount);
        }
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        if remaining_supply > 0 {
            let address = Address::try_from(&PrivateKey::<CurrentNetwork>::new(rng).unwrap()).unwrap();
            public_balances.insert(address, remaining_supply);
        }

        // Construct the bonded balances.
        let bonded_balances = validators
            .iter()
            .map(|(private_key, (amount, _))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (address, *amount))
            })
            .collect();

        // Construct the genesis block, which should pass.
        let block = vm
            .genesis_quorum(
                validators.keys().next().unwrap(),
                Committee::new_genesis(committee_map).unwrap(),
                public_balances,
                bonded_balances,
                rng,
            )
            .unwrap();

        // Add the block.
        vm.add_next_block(&block).unwrap();
    }

    // Note that the maximum delegator size is large enough that the ratification ID cannot be computed.
    #[test]
    fn test_ratify_genesis_greater_than_max_delegator_size() {
        // Initialize an RNG.
        let rng = &mut TestRng::default();

        // Initialize the VM.
        let vm =
            VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None).unwrap()).unwrap();

        // Construct the validators.
        let validators = (0..Committee::<CurrentNetwork>::MAX_COMMITTEE_SIZE)
            .map(|_| {
                let private_key = PrivateKey::<CurrentNetwork>::new(rng).unwrap();
                let amount = MIN_VALIDATOR_STAKE;
                let is_open = true;
                (private_key, (amount, is_open))
            })
            .collect::<IndexMap<_, _>>();

        // Construct the delegators, greater than the maximum delegator size.
        let delegators = (0..MAX_DELEGATORS + 1)
            .map(|_| {
                let private_key = PrivateKey::<CurrentNetwork>::new(rng).unwrap();
                let validator = Address::try_from(validators.keys().next().unwrap()).unwrap();
                let amount = MIN_DELEGATOR_STAKE;
                (private_key, (validator, amount))
            })
            .collect::<IndexMap<_, _>>();

        // Track the allocated amount.
        let mut allocated_amount = 0;

        // Construct the committee.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, _)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            allocated_amount += amount;
            committee_map.insert(address, (*amount, true));
        }
        for (_, (validator, amount)) in &delegators {
            allocated_amount += amount;
            let total_amount = committee_map.get(validator).unwrap().0 + amount;
            committee_map.insert(*validator, (total_amount, true));
        }

        // Construct the public balances, allocating the remaining supply to the validators and zero to the delegators.
        let mut public_balances = IndexMap::new();
        for (private_key, _) in &delegators {
            let address = Address::try_from(private_key).unwrap();
            public_balances.insert(address, 0);
        }
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        for (private_key, _) in &validators {
            let address = Address::try_from(private_key).unwrap();
            let amount = remaining_supply / validators.len() as u64;
            allocated_amount += amount;
            public_balances.insert(address, amount);
        }
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        if remaining_supply > 0 {
            let address = Address::try_from(&PrivateKey::<CurrentNetwork>::new(rng).unwrap()).unwrap();
            public_balances.insert(address, remaining_supply);
        }

        // Construct the bonded balances.
        let mut bonded_balances = validators
            .iter()
            .map(|(private_key, (amount, _))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (address, *amount))
            })
            .collect::<IndexMap<_, _>>();
        for (private_key, (validator, amount)) in &delegators {
            let address = Address::try_from(private_key).unwrap();
            bonded_balances.insert(address, (*validator, *amount));
        }

        // Construct the genesis block, which should fail.
        let result = vm.genesis_quorum(
            validators.keys().next().unwrap(),
            Committee::new_genesis(committee_map).unwrap(),
            public_balances,
            bonded_balances,
            rng,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_ratify_genesis_is_correct() {
        const NUM_VALIDATORS: usize = 5;
        const NUM_DELEGATORS: usize = 8;

        // Sample an RNG.
        let rng = &mut TestRng::default();

        println!("Initializing VMs.");

        // Initialize the VM.
        let vm =
            VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None).unwrap()).unwrap();

        println!("Constructing validator and delegator sets.");

        // Sample the validators.
        let validators: IndexMap<_, _> = (0..NUM_VALIDATORS)
            .map(|_| {
                let private_key = PrivateKey::new(rng).unwrap();
                let amount = MIN_VALIDATOR_STAKE;
                let is_open = true;
                (private_key, (amount, is_open))
            })
            .collect();

        // Sample the delegators, cycling through the validators.
        let delegators: IndexMap<_, _> = (0..NUM_DELEGATORS)
            .map(|i| {
                let private_key = PrivateKey::new(rng).unwrap();
                let validator = Address::try_from(validators.keys().nth(i % NUM_VALIDATORS).unwrap()).unwrap();
                let amount = MIN_DELEGATOR_STAKE;
                (private_key, (validator, amount))
            })
            .collect();

        // Sample a genesis block without any delegators.
        // Specifically, the genesis block will contain a `Ratification` with:
        //   - the committee state, containing only the validator amounts.
        //   - the public balances for the delegators, with 10_000_000u64 microcredits each (plus 843_880u64 microcredits for fees).
        //   - the public balances for the validators dividing up the remaining starting supply.
        //   - the bonded balances, only containing the validators.

        println!("Initializing the VM.");

        // Track the allocated supply.
        let mut allocated_supply = 0;

        // Construct the committee.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, _)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            committee_map.insert(address, *amount);
            allocated_supply += *amount;
        }
        for (private_key, (validator, amount)) in &delegators {
            let _address = Address::try_from(private_key).unwrap();
            let total_amount = committee_map.get(validator).unwrap() + amount;
            committee_map.insert(*validator, total_amount);
            allocated_supply += amount;
        }
        let committee =
            Committee::new_genesis(committee_map.iter().map(|(address, amount)| (*address, (*amount, true))).collect())
                .unwrap();

        // Construct the public balances, allocating the remaining supply to the validators and zero to the delegators.
        let mut public_balances = IndexMap::new();
        for (private_key, (_validator, _amount)) in &delegators {
            let address = Address::try_from(private_key).unwrap();
            public_balances.insert(address, 0u64);
        }
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_supply;
        for private_key in validators.keys() {
            let address = Address::try_from(private_key).unwrap();
            let amount = remaining_supply / NUM_VALIDATORS as u64;
            public_balances.insert(address, amount);
            allocated_supply += amount;
        }
        let address = Address::try_from(validators.keys().next().unwrap()).unwrap();
        let amount = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_supply;
        public_balances.entry(address).and_modify(|balance| *balance += amount).or_insert(amount);

        // Construct the bonded balances.
        let bonded_balances = validators
            .iter()
            .map(|(private_key, (amount, _))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (address, *amount))
            })
            .chain(delegators.iter().map(|(private_key, (validator, amount))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (*validator, *amount))
            }))
            .collect::<IndexMap<_, _>>();

        println!("Generating the genesis block.");

        let genesis = vm
            .genesis_quorum(
                validators.keys().next().unwrap(),
                committee.clone(),
                public_balances.clone(),
                bonded_balances.clone(),
                rng,
            )
            .unwrap();

        println!("Adding the genesis block to the VM.");

        // Add the genesis block to the VM.
        vm.add_next_block(&genesis).unwrap();

        // Check that the state of the `credits.aleo` program is correct.
        let program_id = ProgramID::from_str("credits.aleo").unwrap();
        let committee_mapping_name = Identifier::from_str("committee").unwrap();
        let account_mapping_name = Identifier::from_str("account").unwrap();
        let bonded_mapping_name = Identifier::from_str("bonded").unwrap();
        let metadata_mapping_name = Identifier::from_str("metadata").unwrap();
        let unbonding_mapping_name = Identifier::from_str("unbonding").unwrap();

        // Get and check the committee mapping.
        let actual_committee = vm.finalize_store().get_mapping_confirmed(program_id, committee_mapping_name).unwrap();
        let expected_committee = committee
            .members()
            .iter()
            .map(|(address, (amount, is_open))| {
                (
                    Plaintext::from_str(&address.to_string()).unwrap(),
                    Value::from_str(&format!("{{ microcredits: {amount}u64, is_open: {is_open} }}")).unwrap(),
                )
            })
            .collect_vec();
        // Note that `actual_committee` and `expected_committee` are vectors and not necessarily in the same order.
        // By checking that the lengths of the vector are equal and that all entries in `actual_committee` are in `expected_committee`,
        // we can ensure that the two vectors contain the same data.
        assert_eq!(actual_committee.len(), expected_committee.len());
        for entry in actual_committee.iter() {
            assert!(expected_committee.contains(entry));
        }

        // Get and check the account mapping.
        let actual_account = vm.finalize_store().get_mapping_confirmed(program_id, account_mapping_name).unwrap();
        let expected_account = public_balances
            .iter()
            .map(|(address, amount)| {
                (Plaintext::from_str(&address.to_string()).unwrap(), Value::from_str(&format!("{amount}u64")).unwrap())
            })
            .collect_vec();
        // Note that `actual_account` and `expected_account` are vectors and not necessarily in the same order.
        // By checking that the lengths of the vector are equal and that all entries in `actual_account` are in `expected_account`,
        // we can ensure that the two vectors contain the same data.
        assert_eq!(actual_account.len(), expected_account.len());
        // Check that all entries except for the first validator are the same.
        for entry in actual_account.iter() {
            let first_validator = Address::try_from(validators.keys().next().unwrap()).unwrap();
            // Note that the first validator is used to execute additional transactions in `VM::genesis_quorum`.
            // Therefore, the balance of the first validator will be different from the expected balance.
            if entry.0 == Plaintext::from_str(&first_validator.to_string()).unwrap() {
                assert_eq!(entry.1, Value::from_str(&format!("294999983894244u64")).unwrap());
            } else {
                assert!(expected_account.contains(entry));
            }
        }

        // Get and check the bonded mapping.
        let actual_bonded = vm.finalize_store().get_mapping_confirmed(program_id, bonded_mapping_name).unwrap();
        let expected_bonded = bonded_balances
            .iter()
            .map(|(address, (validator, amount))| {
                (
                    Plaintext::from_str(&address.to_string()).unwrap(),
                    Value::from_str(&format!("{{ validator: {validator}, microcredits: {amount}u64 }}")).unwrap(),
                )
            })
            .collect_vec();
        // Note that `actual_bonded` and `expected_bonded` are vectors and not necessarily in the same order.
        // By checking that the lengths of the vector are equal and that all entries in `actual_bonded` are in `expected_bonded`,
        // we can ensure that the two vectors contain the same data.
        assert_eq!(actual_bonded.len(), expected_bonded.len());
        for entry in actual_bonded.iter() {
            assert!(expected_bonded.contains(entry));
        }

        // Get and check the entry in metadata mapping corresponding to the number of validators.
        let num_validators = vm
            .finalize_store()
            .get_value_confirmed(
                program_id,
                metadata_mapping_name,
                &Plaintext::from_str("aleo1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq3ljyzc").unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(num_validators, Value::from_str(&format!("{NUM_VALIDATORS}u32")).unwrap());

        // Get and check the entry in metadata mapping corresponding to the number of delegators.
        let num_delegators = vm
            .finalize_store()
            .get_value_confirmed(
                program_id,
                metadata_mapping_name,
                &Plaintext::from_str("aleo1qgqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqanmpl0").unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(num_delegators, Value::from_str(&format!("{NUM_DELEGATORS}u32")).unwrap());

        // Get and check the unbonding mapping.
        let actual_unbonding = vm.finalize_store().get_mapping_confirmed(program_id, unbonding_mapping_name).unwrap();
        assert!(actual_unbonding.is_empty());
    }

    #[test]
    fn test_ratify_genesis_is_consistent() {
        const NUM_VALIDATORS: usize = 5;
        const NUM_DELEGATORS: usize = 8;

        // Sample an RNG.
        let rng = &mut TestRng::default();

        println!("Initializing VMs.");

        // Initialize two VMs.
        let vm_1 =
            VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None).unwrap()).unwrap();
        let vm_2 =
            VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None).unwrap()).unwrap();

        println!("Constructing validator and delegator sets.");

        // Sample the validators.
        let validators: IndexMap<_, _> = (0..NUM_VALIDATORS)
            .map(|_| {
                let private_key = PrivateKey::new(rng).unwrap();
                let amount = MIN_VALIDATOR_STAKE;
                let is_open = true;
                (private_key, (amount, is_open))
            })
            .collect();

        // Sample the delegators, cycling through the validators.
        let delegators: IndexMap<_, _> = (0..NUM_DELEGATORS)
            .map(|i| {
                let private_key = PrivateKey::new(rng).unwrap();
                let validator = Address::try_from(validators.keys().nth(i % NUM_VALIDATORS).unwrap()).unwrap();
                let amount = MIN_DELEGATOR_STAKE;
                (private_key, (validator, amount))
            })
            .collect();

        // For the first VM, sample a genesis block without any delegators.
        // Specifically, the genesis block will contain a `Ratification` with:
        //   - the committee state, containing only the validator amounts.
        //   - the public balances for the delegators, with 10_000_000u64 microcredits each (plus 843_880u64 microcredits for fees).
        //   - the public balances for the validators dividing up the remaining starting supply.
        //   - the bonded balances, only containing the validators.

        println!("Initializing the first VM.");

        // Track the allocated supply.
        let mut allocated_supply = 0;

        // Construct the committee.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, _)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            committee_map.insert(address, *amount);
            allocated_supply += *amount;
        }
        let committee =
            Committee::new_genesis(committee_map.iter().map(|(address, amount)| (*address, (*amount, true))).collect())
                .unwrap();

        // Construct the public balances.
        let mut public_balances = IndexMap::new();
        for (private_key, (_validator, _amount)) in &delegators {
            let address = Address::try_from(private_key).unwrap();
            let amount = MIN_DELEGATOR_STAKE * 2;
            public_balances.insert(address, amount);
            allocated_supply += amount;
        }
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_supply;
        for private_key in validators.keys() {
            let address = Address::try_from(private_key).unwrap();
            let amount = remaining_supply / NUM_VALIDATORS as u64;
            public_balances.insert(address, amount);
            allocated_supply += amount;
        }
        let address = Address::try_from(validators.keys().next().unwrap()).unwrap();
        let amount = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_supply;
        public_balances.entry(address).and_modify(|balance| *balance += amount).or_insert(amount);

        // Construct the bonded balances.
        let bonded_balances = validators
            .iter()
            .map(|(private_key, (amount, _))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (address, *amount))
            })
            .collect();

        println!("[VM1] Generating the genesis block.");

        let genesis_1 = vm_1
            .genesis_quorum(validators.keys().next().unwrap(), committee, public_balances, bonded_balances, rng)
            .unwrap();

        println!("[VM1] Adding the genesis block to the VM.");

        // Add the genesis block to the VM.
        vm_1.add_next_block(&genesis_1).unwrap();

        println!("[VM1] Generating bond transactions for each of the delegators.");

        // Generate bond transactions for each of the delegators.
        let mut transactions = Vec::new();
        for (private_key, (validator, amount)) in &delegators {
            let transaction = vm_1
                .execute(
                    private_key,
                    ("credits.aleo", "bond_public"),
                    vec![
                        Value::<CurrentNetwork>::from_str(&validator.to_string()).unwrap(),
                        Value::<CurrentNetwork>::from_str(&format!("{amount}u64")).unwrap(),
                    ]
                    .into_iter(),
                    None,
                    0,
                    None,
                    rng,
                )
                .unwrap();
            transactions.push(transaction);
        }

        println!("[VM1] Generating the next block.");
        let next_block =
            sample_next_block(&vm_1, validators.keys().next().unwrap(), &transactions, &genesis_1, &mut vec![], rng)
                .unwrap();

        println!("[VM1] Adding the next block to the VM.");
        vm_1.add_next_block(&next_block).unwrap();

        // For the second VM, sample a genesis block with the same validators and delegators.
        // Specifically, the genesis block will contain a `Ratification` with:
        //   - the committee state, containing the total staked amount per validator.
        //   - the public balances for the delegators, with 0 microcredits each.
        //   - the public balances for the validators dividing up the remaining starting supply.
        //   - the bonded balances, containing the validators and delegators.

        println!("Initializing the second VM.");

        // Track the allocated supply.
        let mut allocated_supply = 0;

        // Construct the committee.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, _)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            committee_map.insert(address, *amount);
            allocated_supply += *amount;
        }
        for (private_key, (validator, amount)) in &delegators {
            let _address = Address::try_from(private_key).unwrap();
            let total_amount = committee_map.get(validator).unwrap() + amount;
            committee_map.insert(*validator, total_amount);
            allocated_supply += amount;
        }
        let committee =
            Committee::new_genesis(committee_map.iter().map(|(address, amount)| (*address, (*amount, true))).collect())
                .unwrap();

        // Construct the public balances, allocating the remaining supply to the validators and zero to the delegators.
        let mut public_balances = IndexMap::new();
        for (private_key, (_validator, _amount)) in &delegators {
            let address = Address::try_from(private_key).unwrap();
            public_balances.insert(address, 0u64);
        }
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_supply;
        for private_key in validators.keys() {
            let address = Address::try_from(private_key).unwrap();
            let amount = remaining_supply / NUM_VALIDATORS as u64;
            public_balances.insert(address, amount);
            allocated_supply += amount;
        }
        let address = Address::try_from(validators.keys().next().unwrap()).unwrap();
        let amount = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_supply;
        public_balances.entry(address).and_modify(|balance| *balance += amount).or_insert(amount);

        // Construct the bonded balances.
        let bonded_balances = validators
            .iter()
            .map(|(private_key, (amount, _))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (address, *amount))
            })
            .chain(delegators.iter().map(|(private_key, (validator, amount))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (*validator, *amount))
            }))
            .collect();

        println!("[VM2] Generating the genesis block.");

        // Construct the genesis block.
        let genesis_2 = vm_2
            .genesis_quorum(validators.keys().next().unwrap(), committee, public_balances, bonded_balances, rng)
            .unwrap();

        println!("[VM2] Adding the genesis block to the VM.");

        // Add the genesis block to the VM.
        vm_2.add_next_block(&genesis_2).unwrap();

        println!("Checking that all mappings in `credits.aleo` are equal across the two VMs.");

        // Check that all mappings in `credits.aleo` are equal across the two VMs.
        let program_id = ProgramID::from_str("credits.aleo").unwrap();
        let committee_mapping_name = Identifier::from_str("committee").unwrap();
        let bonded_mapping_name = Identifier::from_str("bonded").unwrap();
        let unbonding_mapping_name = Identifier::from_str("unbonding").unwrap();
        let account_mapping_name = Identifier::from_str("account").unwrap();
        let metadata_mapping_name = Identifier::from_str("metadata").unwrap();

        let committee_1 = vm_1.finalize_store().get_mapping_confirmed(program_id, committee_mapping_name).unwrap();
        let committee_2 = vm_2.finalize_store().get_mapping_confirmed(program_id, committee_mapping_name).unwrap();
        assert_eq!(committee_1, committee_2);

        let bonded_1 = vm_1.finalize_store().get_mapping_confirmed(program_id, bonded_mapping_name).unwrap();
        let bonded_2 = vm_2.finalize_store().get_mapping_confirmed(program_id, bonded_mapping_name).unwrap();
        assert_eq!(bonded_1, bonded_2);

        let unbonding_1 = vm_1.finalize_store().get_mapping_confirmed(program_id, unbonding_mapping_name).unwrap();
        let unbonding_2 = vm_2.finalize_store().get_mapping_confirmed(program_id, unbonding_mapping_name).unwrap();
        assert_eq!(unbonding_1, unbonding_2);

        // Check that the account mapping across both VMs have the same keys.
        let account_1 = vm_1
            .finalize_store()
            .get_mapping_confirmed(program_id, account_mapping_name)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k.to_string())
            .collect::<std::collections::HashSet<_>>();
        let account_2 = vm_2
            .finalize_store()
            .get_mapping_confirmed(program_id, account_mapping_name)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k.to_string())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(account_1, account_2);

        // Check that the metadata mapping across both VMs are equal.
        let metadata_1 = vm_1.finalize_store().get_mapping_confirmed(program_id, metadata_mapping_name).unwrap();
        let metadata_2 = vm_2.finalize_store().get_mapping_confirmed(program_id, metadata_mapping_name).unwrap();
        assert_eq!(metadata_1, metadata_2);
    }

    #[test]
    fn test_ratify_genesis_with_insufficient_validator_balance() {
        // Sample an RNG.
        let rng = &mut TestRng::default();

        // Initialize the VM.
        let vm =
            VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None).unwrap()).unwrap();

        // Attempt to construct a genesis quorum, with a validator with an insufficient amount.
        let mut validators = (0..3)
            .map(|_| {
                let private_key = PrivateKey::<CurrentNetwork>::new(rng).unwrap();
                let address = Address::try_from(&private_key).unwrap();
                let amount = MIN_VALIDATOR_STAKE;
                let is_open = true;
                (address, (amount, is_open))
            })
            .collect::<IndexMap<_, _>>();
        validators.insert(Address::try_from(PrivateKey::new(rng).unwrap()).unwrap(), (MIN_VALIDATOR_STAKE - 1, true));

        // Construct the committee.
        let result = Committee::new_genesis(validators);
        assert!(result.is_err());

        // Track the allocated amount.
        let mut allocated_amount = 0;

        // Reset the validators.
        let validators = (0..4)
            .map(|_| {
                let private_key = PrivateKey::new(rng).unwrap();
                let amount = MIN_VALIDATOR_STAKE;
                let is_open = true;
                (private_key, (amount, is_open))
            })
            .collect::<IndexMap<_, _>>();

        // Construct the committee.
        let committee = Committee::new_genesis(
            validators
                .iter()
                .map(|(private_key, (amount, _))| {
                    let address = Address::try_from(private_key).unwrap();
                    allocated_amount += *amount;
                    (address, (*amount, true))
                })
                .collect(),
        )
        .unwrap();

        // Construct the public balances, allocating the remaining supply to rest of the validators.
        let mut public_balances = IndexMap::new();
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        for (private_key, _) in &validators {
            let address = Address::try_from(private_key).unwrap();
            let amount = remaining_supply / validators.len() as u64;
            allocated_amount += amount;
            public_balances.insert(address, amount);
        }
        let address = Address::try_from(validators.keys().next().unwrap()).unwrap();
        let amount = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        public_balances.entry(address).and_modify(|balance| *balance += amount).or_insert(amount);

        // Construct the bonded balances.
        let bonded_balances = validators
            .iter()
            .map(|(private_key, (amount, _))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (address, *amount))
            })
            .collect();

        // Construct the genesis block, which should pass.
        let block = vm
            .genesis_quorum(validators.keys().next().unwrap(), committee, public_balances, bonded_balances, rng)
            .unwrap();

        // Add the block.
        vm.add_next_block(&block).unwrap();
    }

    #[test]
    fn test_ratify_genesis_with_insufficient_delegator_balance() {
        // Sample an RNG.
        let rng = &mut TestRng::default();

        // Initialize the VM.
        let vm =
            VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None).unwrap()).unwrap();

        // Track the allocated amount.
        let mut allocated_amount = 0;

        // Attempt to construct a genesis quorum, with a delegator with an insufficient amount.
        let validators = (0..4)
            .map(|_| {
                let private_key = PrivateKey::new(rng).unwrap();
                let amount = MIN_VALIDATOR_STAKE;
                let is_open = true;
                (private_key, (amount, is_open))
            })
            .collect::<IndexMap<_, _>>();

        let mut delegators = IndexMap::new();
        delegators.insert(
            PrivateKey::new(rng).unwrap(),
            (Address::try_from(validators.keys().next().unwrap()).unwrap(), MIN_DELEGATOR_STAKE - 1),
        );

        // Construct the committee.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, _)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            let amount = if address == Address::try_from(validators.keys().next().unwrap()).unwrap() {
                *amount + MIN_DELEGATOR_STAKE - 1
            } else {
                *amount
            };
            committee_map.insert(address, (amount, true));
            allocated_amount += amount;
        }
        let committee = Committee::new_genesis(committee_map).unwrap();

        // Construct the public balances, allocating the remaining supply to rest of the validators.
        let mut public_balances = IndexMap::new();
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        for (private_key, _) in &validators {
            let address = Address::try_from(private_key).unwrap();
            let amount = remaining_supply / validators.len() as u64;
            allocated_amount += amount;
            public_balances.insert(address, amount);
        }
        let address = Address::try_from(validators.keys().next().unwrap()).unwrap();
        let amount = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        public_balances.entry(address).and_modify(|balance| *balance += amount).or_insert(amount);
        let address = Address::try_from(delegators.keys().next().unwrap()).unwrap();
        public_balances.insert(address, 0);

        // Construct the bonded balances.
        let bonded_balances = validators
            .iter()
            .map(|(private_key, (amount, _))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (address, *amount))
            })
            .chain(delegators.iter().map(|(private_key, (validator, amount))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (*validator, *amount))
            }))
            .collect();

        // Construct the genesis block, which should fail.
        let result =
            vm.genesis_quorum(validators.keys().next().unwrap(), committee, public_balances, bonded_balances, rng);
        assert!(result.is_err());

        // Reset the delegators.
        let mut delegators = IndexMap::new();
        delegators.insert(
            PrivateKey::new(rng).unwrap(),
            (Address::try_from(validators.keys().next().unwrap()).unwrap(), MIN_DELEGATOR_STAKE),
        );

        // Track the allocated amount.
        let mut allocated_amount = 0;

        // Construct the committee.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, _)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            let amount = if address == Address::try_from(validators.keys().next().unwrap()).unwrap() {
                *amount + MIN_DELEGATOR_STAKE
            } else {
                *amount
            };
            committee_map.insert(address, (amount, true));
            allocated_amount += amount;
        }
        let committee = Committee::new_genesis(committee_map).unwrap();

        // Construct the public balances, allocating the remaining supply to rest of the validators.
        let mut public_balances = IndexMap::new();
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        for (private_key, _) in &validators {
            let address = Address::try_from(private_key).unwrap();
            let amount = remaining_supply / validators.len() as u64;
            allocated_amount += amount;
            public_balances.insert(address, amount);
        }
        let address = Address::try_from(validators.keys().next().unwrap()).unwrap();
        let amount = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        public_balances.entry(address).and_modify(|balance| *balance += amount).or_insert(amount);
        let address = Address::try_from(delegators.keys().next().unwrap()).unwrap();
        public_balances.insert(address, 0);

        // Construct the bonded balances.
        let bonded_balances = validators
            .iter()
            .map(|(private_key, (amount, _))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (address, *amount))
            })
            .chain(delegators.iter().map(|(private_key, (validator, amount))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (*validator, *amount))
            }))
            .collect();

        // Construct the genesis block, which should pass.
        let block = vm
            .genesis_quorum(validators.keys().next().unwrap(), committee, public_balances, bonded_balances, rng)
            .unwrap();

        // Add the block.
        vm.add_next_block(&block).unwrap();
    }

    #[test]
    fn test_ratify_genesis_with_incorrect_committee_amounts() {
        // Sample an RNG.
        let rng = &mut TestRng::default();

        // Initialize the VM.
        let vm =
            VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None).unwrap()).unwrap();

        // Track the allocated amount.
        let mut allocated_amount = 0;

        // Initialize the validators.
        let validators = (0..4)
            .map(|_| {
                let private_key = PrivateKey::new(rng).unwrap();
                let amount = MIN_VALIDATOR_STAKE;
                let is_open = true;
                (private_key, (amount, is_open))
            })
            .collect::<IndexMap<_, _>>();

        // Initialize the delegators.
        let delegators = (0..4)
            .map(|_| {
                let private_key = PrivateKey::new(rng).unwrap();
                let validator = Address::try_from(validators.keys().next().unwrap()).unwrap();
                let amount = MIN_DELEGATOR_STAKE;
                (private_key, (validator, amount))
            })
            .collect::<IndexMap<_, _>>();

        // Construct the **incorrect** committee.
        // Note that this committee is missing the additional stake from the delegators.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, _)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            committee_map.insert(address, (*amount, true));
            allocated_amount += amount;
        }
        let committee = Committee::new_genesis(committee_map).unwrap();

        // Construct the public balances, allocating the remaining supply to rest of the validators.
        let mut public_balances = IndexMap::new();
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        for (private_key, _) in &validators {
            let address = Address::try_from(private_key).unwrap();
            let amount = remaining_supply / validators.len() as u64;
            allocated_amount += amount;
            public_balances.insert(address, amount);
        }
        let address = Address::try_from(validators.keys().next().unwrap()).unwrap();
        let amount = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        public_balances.entry(address).and_modify(|balance| *balance += amount).or_insert(amount);
        for (private_key, _) in &delegators {
            let address = Address::try_from(private_key).unwrap();
            public_balances.insert(address, 0);
        }

        // Construct the bonded balances.
        let bonded_balances: IndexMap<_, _> = validators
            .iter()
            .map(|(private_key, (amount, _))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (address, *amount))
            })
            .chain(delegators.iter().map(|(private_key, (validator, amount))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (*validator, *amount))
            }))
            .collect();

        // Construct the genesis block, which should fail.
        let result = vm.genesis_quorum(
            validators.keys().next().unwrap(),
            committee,
            public_balances,
            bonded_balances.clone(),
            rng,
        );
        assert!(result.is_err());

        // Reset the tracked amount.
        let mut allocated_amount = 0;

        // Construct the **correct** committee.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, _)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            committee_map.insert(address, (*amount, true));
            allocated_amount += amount;
        }
        for (_, (validator, amount)) in &delegators {
            let total_amount = committee_map.get(validator).unwrap().0 + amount;
            committee_map.insert(*validator, (total_amount, true));
            allocated_amount += amount;
        }
        let committee = Committee::new_genesis(committee_map).unwrap();

        // Construct the public balances, allocating the remaining supply to rest of the validators.
        let mut public_balances = IndexMap::new();
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        for (private_key, _) in &validators {
            let address = Address::try_from(private_key).unwrap();
            let amount = remaining_supply / validators.len() as u64;
            allocated_amount += amount;
            public_balances.insert(address, amount);
        }
        let address = Address::try_from(validators.keys().next().unwrap()).unwrap();
        let amount = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        public_balances.entry(address).and_modify(|balance| *balance += amount).or_insert(amount);
        for (private_key, _) in &delegators {
            let address = Address::try_from(private_key).unwrap();
            public_balances.insert(address, 0);
        }

        // Construct the genesis block, which should pass.
        let block = vm
            .genesis_quorum(validators.keys().next().unwrap(), committee, public_balances, bonded_balances, rng)
            .unwrap();

        // Add the block.
        vm.add_next_block(&block).unwrap();
    }

    #[test]
    fn test_ratify_genesis_with_closed_validator() {
        // Sample an RNG.
        let rng = &mut TestRng::default();

        // Initialize the VM.
        let vm =
            VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None).unwrap()).unwrap();

        // Initialize the validators, with one closed.
        let validators = (0..4)
            .map(|i| {
                let private_key = PrivateKey::new(rng).unwrap();
                let amount = MIN_VALIDATOR_STAKE;
                let is_open = i != 0;
                (private_key, (amount, is_open))
            })
            .collect::<IndexMap<_, _>>();

        // Initialize a potential delegator.
        let delegator_key = PrivateKey::new(rng).unwrap();
        let delegator_address = Address::try_from(delegator_key).unwrap();

        // Track the allocated amount.
        let mut allocated_amount = 0;

        // Construct the committee.
        let mut committee_map = IndexMap::new();
        for (private_key, (amount, is_open)) in &validators {
            let address = Address::try_from(private_key).unwrap();
            committee_map.insert(address, (*amount, *is_open));
            allocated_amount += *amount;
        }

        // Construct the public balances, allocating half to the first validator and the remaining to the delegator.
        let mut public_balances = IndexMap::new();
        let remaining_supply = <CurrentNetwork as Network>::STARTING_SUPPLY - allocated_amount;
        public_balances.insert(Address::try_from(validators.keys().next().unwrap()).unwrap(), remaining_supply / 2);
        public_balances.insert(delegator_address, remaining_supply - remaining_supply / 2);

        // Construct the bonded balances.
        let bonded_balances = validators
            .iter()
            .map(|(private_key, (amount, _))| {
                let address = Address::try_from(private_key).unwrap();
                (address, (address, *amount))
            })
            .collect();

        // Construct the genesis block, which should pass.
        let block = vm
            .genesis_quorum(
                validators.keys().next().unwrap(),
                Committee::new_genesis(committee_map).unwrap(),
                public_balances,
                bonded_balances,
                rng,
            )
            .unwrap();

        // Add the block.
        vm.add_next_block(&block).unwrap();

        // Attempt to bond the potential delegator to the closed validator.
        let transaction = vm
            .execute(
                &delegator_key,
                ("credits.aleo", "bond_public"),
                vec![
                    Value::<CurrentNetwork>::from_str(
                        &Address::try_from(validators.keys().next().unwrap()).unwrap().to_string(),
                    )
                    .unwrap(),
                    Value::<CurrentNetwork>::from_str(&format!("{MIN_DELEGATOR_STAKE}u64")).unwrap(),
                ]
                .into_iter(),
                None,
                0,
                None,
                rng,
            )
            .unwrap();

        // Generate the next block.
        let next_block =
            sample_next_block(&vm, validators.keys().next().unwrap(), &vec![transaction], &block, &mut vec![], rng)
                .unwrap();

        // Add the next block.
        vm.add_next_block(&next_block).unwrap();

        // Check that the delegator is not in the `bonded` mapping.
        let bonded_mapping = vm
            .finalize_store()
            .get_mapping_confirmed(
                ProgramID::from_str("credits.aleo").unwrap(),
                Identifier::from_str("bonded").unwrap(),
            )
            .unwrap();
        assert_eq!(bonded_mapping.len(), validators.len());

        // Attempt to bond the potential delegator to the open validator.
        let transaction = vm
            .execute(
                &delegator_key,
                ("credits.aleo", "bond_public"),
                vec![
                    Value::<CurrentNetwork>::from_str(
                        &Address::try_from(validators.keys().nth(1).unwrap()).unwrap().to_string(),
                    )
                    .unwrap(),
                    Value::<CurrentNetwork>::from_str(&format!("{MIN_DELEGATOR_STAKE}u64")).unwrap(),
                ]
                .into_iter(),
                None,
                0,
                None,
                rng,
            )
            .unwrap();

        // Generate the next block.
        let next_block = sample_next_block(
            &vm,
            validators.keys().next().unwrap(),
            &vec![transaction],
            &next_block,
            &mut vec![],
            rng,
        )
        .unwrap();

        // Add the next block.
        vm.add_next_block(&next_block).unwrap();

        // Check that the delegator is in the `bonded` mapping.
        let bonded_mapping = vm
            .finalize_store()
            .get_mapping_confirmed(
                ProgramID::from_str("credits.aleo").unwrap(),
                Identifier::from_str("bonded").unwrap(),
            )
            .unwrap();
        assert_eq!(bonded_mapping.len(), validators.len() + 1);
    }
}
