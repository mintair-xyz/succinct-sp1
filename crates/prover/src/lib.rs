//! An end-to-end-prover implementation for the SP1 RISC-V zkVM.
//!
//! Separates the proof generation process into multiple stages:
//!
//! 1. Generate shard proofs which split up and prove the valid execution of a RISC-V program.
//! 2. Compress shard proofs into a single shard proof.
//! 3. Wrap the shard proof into a SNARK-friendly field.
//! 4. Wrap the last shard proof, proven over the SNARK-friendly field, into a PLONK proof.

#![allow(clippy::too_many_arguments)]
#![allow(clippy::new_without_default)]
#![allow(clippy::collapsible_else_if)]

pub mod build;
pub mod components;
pub mod gas;
pub mod shapes;
pub mod types;
pub mod utils;
pub mod verify;

use std::{
    borrow::Borrow,
    collections::BTreeMap,
    env,
    error::Error,
    num::NonZeroUsize,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{channel, sync_channel},
        Arc, Mutex, OnceLock,
    },
    thread,
};

use crate::shapes::SP1CompressProgramShape;
use lru::LruCache;
use p3_baby_bear::BabyBear;
use p3_field::{AbstractField, PrimeField, PrimeField32};
use p3_matrix::dense::RowMajorMatrix;
use shapes::SP1ProofShape;
use sp1_core_executor::{
    estimator::RecordEstimator, ExecutionError, ExecutionReport, Executor, Program, RiscvAirId,
    SP1Context,
};
use sp1_core_machine::{
    io::SP1Stdin,
    reduce::SP1ReduceProof,
    riscv::RiscvAir,
    shape::CoreShapeConfig,
    utils::{concurrency::TurnBasedSync, SP1CoreProverError},
};
use sp1_primitives::hash_deferred_proof;
pub use sp1_primitives::io::SP1PublicValues;
use sp1_recursion_circuit::{
    hash::FieldHasher,
    machine::{
        PublicValuesOutputDigest, SP1CompressRootVerifierWithVKey, SP1CompressShape,
        SP1CompressWithVKeyVerifier, SP1CompressWithVKeyWitnessValues, SP1CompressWithVkeyShape,
        SP1CompressWitnessValues, SP1DeferredVerifier, SP1DeferredWitnessValues,
        SP1MerkleProofWitnessValues, SP1RecursionShape, SP1RecursionWitnessValues,
        SP1RecursiveVerifier,
    },
    merkle_tree::MerkleTree,
    witness::Witnessable,
    WrapConfig,
};
use sp1_recursion_compiler::{
    circuit::AsmCompiler,
    config::InnerConfig,
    ir::{Builder, DslIrProgram, Witness},
};
use sp1_recursion_core::{
    air::RecursionPublicValues,
    machine::RecursionAir,
    runtime::ExecutionRecord,
    shape::{RecursionShape, RecursionShapeConfig},
    stark::BabyBearPoseidon2Outer,
    RecursionProgram, Runtime as RecursionRuntime,
};
pub use sp1_recursion_gnark_ffi::proof::{Groth16Bn254Proof, PlonkBn254Proof};
use sp1_recursion_gnark_ffi::{groth16_bn254::Groth16Bn254Prover, plonk_bn254::PlonkBn254Prover};
use sp1_stark::{
    baby_bear_poseidon2::BabyBearPoseidon2,
    shape::{OrderedShape, Shape},
    Challenge, MachineProver, MachineProvingKey, SP1ProverOpts, ShardProof, SplitOpts,
    StarkGenericConfig, StarkVerifyingKey, Val, Word, DIGEST_SIZE,
};
use tracing::instrument;

pub use types::*;
use utils::{sp1_committed_values_digest_bn254, sp1_vkey_digest_bn254, words_to_bytes};

use components::{CpuProverComponents, SP1ProverComponents};

/// The global version for all components of SP1.
///
/// This string should be updated whenever any step in verifying an SP1 proof changes, including
/// core, recursion, and plonk-bn254. This string is used to download SP1 artifacts and the gnark
/// docker image.
pub const SP1_CIRCUIT_VERSION: &str = include_str!("../SP1_VERSION");

/// The configuration for the core prover.
pub type CoreSC = BabyBearPoseidon2;

/// The configuration for the inner prover.
pub type InnerSC = BabyBearPoseidon2;

/// The configuration for the outer prover.
pub type OuterSC = BabyBearPoseidon2Outer;

pub type DeviceProvingKey<C> = <<C as SP1ProverComponents>::CoreProver as MachineProver<
    BabyBearPoseidon2,
    RiscvAir<BabyBear>,
>>::DeviceProvingKey;

const COMPRESS_DEGREE: usize = 3;
const SHRINK_DEGREE: usize = 3;
const WRAP_DEGREE: usize = 9;

const CORE_CACHE_SIZE: usize = 5;
pub const REDUCE_BATCH_SIZE: usize = 2;

pub type CompressAir<F> = RecursionAir<F, COMPRESS_DEGREE>;
pub type ShrinkAir<F> = RecursionAir<F, SHRINK_DEGREE>;
pub type WrapAir<F> = RecursionAir<F, WRAP_DEGREE>;

/// A end-to-end for the SP1 RISC-V zkVM.
///
/// This object coordinates the proving along all the steps: core, compression, shrinkage, and
/// wrapping.
pub struct SP1Prover<C: SP1ProverComponents = CpuProverComponents> {
    /// The core prover.
    pub core_prover: C::CoreProver,
    /// The compress prover (for both lift and join).
    pub compress_prover: C::CompressProver,
    /// The shrink prover.
    pub shrink_prover: C::ShrinkProver,
    /// The wrap prover.
    pub wrap_prover: C::WrapProver,
    /// The cache of compiled recursion programs.
    pub lift_programs_lru: Mutex<LruCache<SP1RecursionShape, Arc<RecursionProgram<BabyBear>>>>,
    /// The number of cache misses for recursion programs.
    pub lift_cache_misses: AtomicUsize,
    /// The cache of compiled compression programs.
    pub join_programs_map: BTreeMap<SP1CompressWithVkeyShape, Arc<RecursionProgram<BabyBear>>>,
    /// The number of cache misses for compression programs.
    pub join_cache_misses: AtomicUsize,
    /// The root of the allowed recursion verification keys.
    pub recursion_vk_root: <InnerSC as FieldHasher<BabyBear>>::Digest,
    /// The allowed VKs and their corresponding indices.
    pub recursion_vk_map: BTreeMap<<InnerSC as FieldHasher<BabyBear>>::Digest, usize>,
    /// The Merkle tree for the allowed VKs.
    pub recursion_vk_tree: MerkleTree<BabyBear, InnerSC>,
    /// The core shape configuration.
    pub core_shape_config: Option<CoreShapeConfig<BabyBear>>,
    /// The recursion shape configuration.
    pub compress_shape_config: Option<RecursionShapeConfig<BabyBear, CompressAir<BabyBear>>>,
    /// The program for wrapping.
    pub wrap_program: OnceLock<Arc<RecursionProgram<BabyBear>>>,
    /// The verifying key for wrapping.
    pub wrap_vk: OnceLock<StarkVerifyingKey<OuterSC>>,
    /// Whether to verify verification keys.
    pub vk_verification: bool,
}

impl<C: SP1ProverComponents> SP1Prover<C> {
    /// Initializes a new [SP1Prover].
    #[instrument(name = "initialize prover", level = "debug", skip_all)]
    pub fn new() -> Self {
        Self::uninitialized()
    }

    /// Creates a new [SP1Prover] with lazily initialized components.
    pub fn uninitialized() -> Self {
        // Initialize the provers.
        let core_machine = RiscvAir::machine(CoreSC::default());
        let core_prover = C::CoreProver::new(core_machine);

        let compress_machine = CompressAir::compress_machine(InnerSC::default());
        let compress_prover = C::CompressProver::new(compress_machine);

        let shrink_machine = ShrinkAir::shrink_machine(InnerSC::compressed());
        let shrink_prover = C::ShrinkProver::new(shrink_machine);

        let wrap_machine = WrapAir::wrap_machine(OuterSC::default());
        let wrap_prover = C::WrapProver::new(wrap_machine);

        let core_cache_size = NonZeroUsize::new(
            env::var("PROVER_CORE_CACHE_SIZE")
                .unwrap_or_else(|_| CORE_CACHE_SIZE.to_string())
                .parse()
                .unwrap_or(CORE_CACHE_SIZE),
        )
        .expect("PROVER_CORE_CACHE_SIZE must be a non-zero usize");

        let core_shape_config = env::var("FIX_CORE_SHAPES")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(true)
            .then_some(CoreShapeConfig::default());

        let recursion_shape_config = env::var("FIX_RECURSION_SHAPES")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(true)
            .then_some(RecursionShapeConfig::default());

        let vk_verification =
            env::var("VERIFY_VK").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(true);
        tracing::debug!("vk verification: {}", vk_verification);

        // Read the shapes from the shapes directory and deserialize them into memory.
        let allowed_vk_map: BTreeMap<[BabyBear; DIGEST_SIZE], usize> = if vk_verification {
            bincode::deserialize(include_bytes!(concat!(env!("OUT_DIR"), "/vk_map.bin"))).unwrap()
        } else {
            bincode::deserialize(include_bytes!("vk_map_dummy.bin")).unwrap()
        };

        let (root, merkle_tree) = MerkleTree::commit(allowed_vk_map.keys().copied().collect());

        let mut compress_programs = BTreeMap::new();
        let program_cache_disabled = env::var("SP1_DISABLE_PROGRAM_CACHE")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !program_cache_disabled {
            if let Some(config) = &recursion_shape_config {
                SP1ProofShape::generate_compress_shapes(config, REDUCE_BATCH_SIZE).for_each(
                    |shape| {
                        let compress_shape = SP1CompressWithVkeyShape {
                            compress_shape: shape.into(),
                            merkle_tree_height: merkle_tree.height,
                        };
                        let input = SP1CompressWithVKeyWitnessValues::dummy(
                            compress_prover.machine(),
                            &compress_shape,
                        );
                        let program = compress_program_from_input::<C>(
                            recursion_shape_config.as_ref(),
                            &compress_prover,
                            vk_verification,
                            &input,
                        );
                        let program = Arc::new(program);
                        compress_programs.insert(compress_shape, program);
                    },
                );
            }
        }

        Self {
            core_prover,
            compress_prover,
            shrink_prover,
            wrap_prover,
            lift_programs_lru: Mutex::new(LruCache::new(core_cache_size)),
            lift_cache_misses: AtomicUsize::new(0),
            join_programs_map: compress_programs,
            join_cache_misses: AtomicUsize::new(0),
            recursion_vk_root: root,
            recursion_vk_tree: merkle_tree,
            recursion_vk_map: allowed_vk_map,
            core_shape_config,
            compress_shape_config: recursion_shape_config,
            vk_verification,
            wrap_program: OnceLock::new(),
            wrap_vk: OnceLock::new(),
        }
    }

    /// Creates a proving key and a verifying key for a given RISC-V ELF.
    #[instrument(name = "setup", level = "debug", skip_all)]
    pub fn setup(
        &self,
        elf: &[u8],
    ) -> (SP1ProvingKey, DeviceProvingKey<C>, Program, SP1VerifyingKey) {
        let program = self.get_program(elf).unwrap();
        let (pk, vk) = self.core_prover.setup(&program);
        let vk = SP1VerifyingKey { vk };
        let pk = SP1ProvingKey {
            pk: self.core_prover.pk_to_host(&pk),
            elf: elf.to_vec(),
            vk: vk.clone(),
        };
        let pk_d = self.core_prover.pk_to_device(&pk.pk);
        (pk, pk_d, program, vk)
    }

    /// Get a program with an allowed preprocessed shape.
    pub fn get_program(&self, elf: &[u8]) -> eyre::Result<Program> {
        let mut program = Program::from(elf)?;
        if let Some(core_shape_config) = &self.core_shape_config {
            core_shape_config.fix_preprocessed_shape(&mut program)?;
        }
        Ok(program)
    }

    fn get_gas_calculator(
        &self,
        preprocessed_shape: Shape<RiscvAirId>,
        split_opts: SplitOpts,
    ) -> impl FnMut(&RecordEstimator) -> Result<u64, Box<dyn Error>> + '_ {
        move |estimator: &RecordEstimator| -> Result<u64, Box<dyn Error>> {
            let est_records = gas::estimated_records(&split_opts, estimator);
            let raw_gas =
                gas::fit_records_to_shapes(self.core_shape_config.as_ref().unwrap(), est_records)
                    .enumerate()
                    .map(|(i, shape)| {
                        let mut shape: Shape<RiscvAirId> = shape.map_err(Box::new)?;
                        shape.extend(preprocessed_shape.iter().map(|(k, v)| (*k, *v)));
                        tracing::debug!("shape for estimated shard {i}: {:?}", &shape.inner);
                        Ok(gas::predict(enum_map::EnumMap::from_iter(shape).as_array()))
                    })
                    .sum::<Result<_, Box<dyn Error>>>()?;
            let gas = gas::final_transform(raw_gas).map_err(Box::new)?;
            Ok(gas)
        }
    }

    /// Execute an SP1 program with the specified inputs.
    #[instrument(name = "execute", level = "info", skip_all)]
    pub fn execute<'a>(
        &'a self,
        elf: &[u8],
        stdin: &SP1Stdin,
        mut context: SP1Context<'a>,
    ) -> Result<(SP1PublicValues, [u8; 32], ExecutionReport), ExecutionError> {
        context.subproof_verifier = Some(self);

        let calculate_gas = context.calculate_gas;

        let (opts, program) = if calculate_gas {
            (gas::GAS_OPTS, self.get_program(elf).unwrap())
        } else {
            (sp1_stark::SP1CoreOpts::default(), Program::from(elf).unwrap())
        };
        let preprocessed_shape = program.preprocessed_shape.clone();

        let mut runtime = Executor::with_context(program, opts, context);

        if calculate_gas {
            // Needed to figure out where the shard boundaries are.
            runtime.maximal_shapes = self.core_shape_config.as_ref().map(|config| {
                config.maximal_core_shapes(opts.shard_size.ilog2() as usize).into_iter().collect()
            });
            runtime.record_estimator = Some(Box::default());
        }

        runtime.maybe_setup_profiler(elf);

        runtime.write_vecs(&stdin.buffer);
        for (proof, vkey) in stdin.proofs.iter() {
            runtime.write_proof(proof.clone(), vkey.clone());
        }
        runtime.run_fast()?;

        if calculate_gas {
            let gas = self.get_gas_calculator(preprocessed_shape.unwrap(), opts.split_opts)(
                runtime.record_estimator.as_ref().unwrap(),
            );
            runtime.report.gas = gas
                .inspect(|g| tracing::info!("gas: {}", g))
                .inspect_err(|e| tracing::error!("Encountered error while calculating gas: {}", e))
                .ok();
        }

        let mut committed_value_digest = [0u8; 32];
        runtime.record.public_values.committed_value_digest.iter().enumerate().for_each(
            |(i, word)| {
                let bytes = word.to_le_bytes();
                committed_value_digest[i * 4..(i + 1) * 4].copy_from_slice(&bytes);
            },
        );

        Ok((
            SP1PublicValues::from(&runtime.state.public_values_stream),
            committed_value_digest,
            runtime.report,
        ))
    }

    /// Generate shard proofs which split up and prove the valid execution of a RISC-V program with
    /// the core prover. Uses the provided context.
    #[instrument(name = "prove_core", level = "info", skip_all)]
    pub fn prove_core<'a>(
        &'a self,
        pk_d: &<<C as SP1ProverComponents>::CoreProver as MachineProver<
            BabyBearPoseidon2,
            RiscvAir<BabyBear>,
        >>::DeviceProvingKey,
        program: Program,
        stdin: &SP1Stdin,
        opts: SP1ProverOpts,
        mut context: SP1Context<'a>,
    ) -> Result<SP1CoreProof, SP1CoreProverError> {
        context.subproof_verifier = Some(self);

        // Launch two threads to simultaneously prove the core and compile the first few
        // recursion programs in parallel.
        let span = tracing::Span::current().clone();
        std::thread::scope(|s| {
            let _span = span.enter();
            let (proof_tx, proof_rx) = channel();
            let (shape_tx, shape_rx) = channel();

            let span = tracing::Span::current().clone();
            let handle = s.spawn(move || {
                let _span = span.enter();

                // Copy the proving key to the device.
                let pk = pk_d;

                // We may calculate gas while proving if the opts match the hardcoded variant.
                // This ensures that the gas number is consistent between `execute` and `prove_core`.
                // This behavior is undocumented because it is confusing and not very useful.
                //
                // If `context.calculate_gas` is set, we use the logic from the `gas` module
                // after checkpoint execution to print gas as part of the execution report.
                #[allow(clippy::type_complexity)]
                let gas_calculator = (context.calculate_gas
                    && std::env::var("SP1_FORCE_GAS").is_ok())
                .then(
                    || -> Box<dyn FnOnce(&RecordEstimator) -> Result<u64, Box<dyn Error>> + '_> {
                        tracing::info!("Forcing calculation of gas while proving.");
                        if opts.core_opts == gas::GAS_OPTS {
                            tracing::info!(
                                "The SP1CoreOpts matches the gas opts, so gas will be consistent."
                            );
                        } else {
                            tracing::warn!(
                                "The SP1CoreOpts does not match the gas opts. \
                                Gas will likely disagree with the standard gas calculated when executing."
                            );
                        }
                        let preprocessed_shape = program.preprocessed_shape.clone().unwrap();
                        Box::new(
                            self.get_gas_calculator(preprocessed_shape, opts.core_opts.split_opts),
                        )
                    },
                );

                // Prove the core and stream the proofs and shapes.
                sp1_core_machine::utils::prove_core_stream::<_, C::CoreProver>(
                    &self.core_prover,
                    pk,
                    program,
                    stdin,
                    opts.core_opts,
                    context,
                    self.core_shape_config.as_ref(),
                    proof_tx,
                    shape_tx,
                    None,
                    gas_calculator,
                )
            });

            // Receive the first few shapes and comile the recursion programs.
            for _ in 0..3 {
                if let Ok((shape, is_complete)) = shape_rx.recv() {
                    let recursion_shape =
                        SP1RecursionShape { proof_shapes: vec![shape], is_complete };

                    // Only need to compile the recursion program if we're not in the one-shard
                    // case.
                    let compress_shape = SP1CompressProgramShape::Recursion(recursion_shape);

                    // Insert the program into the cache.
                    self.program_from_shape(compress_shape, None);
                }
            }

            // Collect the shard proofs and the public values stream.
            let shard_proofs: Vec<ShardProof<_>> = proof_rx.iter().collect();
            let (public_values_stream, cycles) = handle.join().unwrap().unwrap();
            let public_values = SP1PublicValues::from(&public_values_stream);
            Self::check_for_high_cycles(cycles);
            Ok(SP1CoreProof {
                proof: SP1CoreProofData(shard_proofs),
                stdin: stdin.clone(),
                public_values,
                cycles,
            })
        })
    }

    /// Reduce shards proofs to a single shard proof using the recursion prover.
    #[instrument(name = "compress", level = "info", skip_all)]
    pub fn compress(
        &self,
        vk: &SP1VerifyingKey,
        proof: SP1CoreProof,
        deferred_proofs: Vec<SP1ReduceProof<InnerSC>>,
        opts: SP1ProverOpts,
    ) -> Result<SP1ReduceProof<InnerSC>, SP1RecursionProverError> {
        #[allow(clippy::type_complexity)]
        enum TracesOrInput {
            ProgramRecordTraces(
                Box<(
                    Arc<RecursionProgram<BabyBear>>,
                    ExecutionRecord<BabyBear>,
                    Vec<(String, RowMajorMatrix<BabyBear>)>,
                )>,
            ),
            CircuitWitness(Box<SP1CircuitWitness>),
        }

        // The batch size for reducing two layers of recursion.
        let batch_size = REDUCE_BATCH_SIZE;
        // The batch size for reducing the first layer of recursion.
        let first_layer_batch_size = 1;

        let shard_proofs = &proof.proof.0;

        // Generate the first layer inputs.
        let first_layer_inputs =
            self.get_first_layer_inputs(vk, shard_proofs, &deferred_proofs, first_layer_batch_size);

        // Calculate the expected height of the tree.
        let mut expected_height = if first_layer_inputs.len() == 1 { 0 } else { 1 };
        let num_first_layer_inputs = first_layer_inputs.len();
        let mut num_layer_inputs = num_first_layer_inputs;
        while num_layer_inputs > batch_size {
            num_layer_inputs = num_layer_inputs.div_ceil(2);
            expected_height += 1;
        }

        // Generate the proofs.
        let span = tracing::Span::current().clone();
        let (vk, proof) = thread::scope(|s| {
            let _span = span.enter();

            // Spawn a worker that sends the first layer inputs to a bounded channel.
            let input_sync = Arc::new(TurnBasedSync::new());
            let (input_tx, input_rx) = sync_channel::<(usize, usize, SP1CircuitWitness, bool)>(
                opts.recursion_opts.checkpoints_channel_capacity,
            );
            let input_tx = Arc::new(Mutex::new(input_tx));
            {
                let input_tx = Arc::clone(&input_tx);
                let input_sync = Arc::clone(&input_sync);
                s.spawn(move || {
                    for (index, input) in first_layer_inputs.into_iter().enumerate() {
                        input_sync.wait_for_turn(index);
                        input_tx.lock().unwrap().send((index, 0, input, false)).unwrap();
                        input_sync.advance_turn();
                    }
                });
            }

            // Spawn workers who generate the records and traces.
            let record_and_trace_sync = Arc::new(TurnBasedSync::new());
            let (record_and_trace_tx, record_and_trace_rx) =
                sync_channel::<(usize, usize, TracesOrInput)>(
                    opts.recursion_opts.records_and_traces_channel_capacity,
                );
            let record_and_trace_tx = Arc::new(Mutex::new(record_and_trace_tx));
            let record_and_trace_rx = Arc::new(Mutex::new(record_and_trace_rx));
            let input_rx = Arc::new(Mutex::new(input_rx));
            for _ in 0..opts.recursion_opts.trace_gen_workers {
                let record_and_trace_sync = Arc::clone(&record_and_trace_sync);
                let record_and_trace_tx = Arc::clone(&record_and_trace_tx);
                let input_rx = Arc::clone(&input_rx);
                let span = tracing::debug_span!("generate records and traces");
                s.spawn(move || {
                    let _span = span.enter();
                    loop {
                        let received = { input_rx.lock().unwrap().recv() };
                        if let Ok((index, height, input, false)) = received {
                            // Get the program and witness stream.
                            let (program, witness_stream) = tracing::debug_span!(
                                "get program and witness stream"
                            )
                            .in_scope(|| match input {
                                SP1CircuitWitness::Core(input) => {
                                    let mut witness_stream = Vec::new();
                                    Witnessable::<InnerConfig>::write(&input, &mut witness_stream);
                                    (self.recursion_program(&input), witness_stream)
                                }
                                SP1CircuitWitness::Deferred(input) => {
                                    let mut witness_stream = Vec::new();
                                    Witnessable::<InnerConfig>::write(&input, &mut witness_stream);
                                    (self.deferred_program(&input), witness_stream)
                                }
                                SP1CircuitWitness::Compress(input) => {
                                    let mut witness_stream = Vec::new();

                                    let input_with_merkle = self.make_merkle_proofs(input);

                                    Witnessable::<InnerConfig>::write(
                                        &input_with_merkle,
                                        &mut witness_stream,
                                    );

                                    (self.compress_program(&input_with_merkle), witness_stream)
                                }
                            });

                            // Execute the runtime.
                            let record = tracing::debug_span!("execute runtime").in_scope(|| {
                                let mut runtime =
                                    RecursionRuntime::<Val<InnerSC>, Challenge<InnerSC>, _>::new(
                                        program.clone(),
                                        self.compress_prover.config().perm.clone(),
                                    );
                                runtime.witness_stream = witness_stream.into();
                                runtime
                                    .run()
                                    .map_err(|e| {
                                        SP1RecursionProverError::RuntimeError(e.to_string())
                                    })
                                    .unwrap();
                                runtime.record
                            });

                            // Generate the dependencies.
                            let mut records = vec![record];
                            tracing::debug_span!("generate dependencies").in_scope(|| {
                                self.compress_prover.machine().generate_dependencies(
                                    &mut records,
                                    &opts.recursion_opts,
                                    None,
                                )
                            });

                            // Generate the traces.
                            let record = records.into_iter().next().unwrap();
                            let traces = tracing::debug_span!("generate traces")
                                .in_scope(|| self.compress_prover.generate_traces(&record));

                            // Wait for our turn to update the state.
                            record_and_trace_sync.wait_for_turn(index);

                            // Send the record and traces to the worker.
                            record_and_trace_tx
                                .lock()
                                .unwrap()
                                .send((
                                    index,
                                    height,
                                    TracesOrInput::ProgramRecordTraces(Box::new((
                                        program, record, traces,
                                    ))),
                                ))
                                .unwrap();

                            // Advance the turn.
                            record_and_trace_sync.advance_turn();
                        } else if let Ok((index, height, input, true)) = received {
                            record_and_trace_sync.wait_for_turn(index);

                            // Send the record and traces to the worker.
                            record_and_trace_tx
                                .lock()
                                .unwrap()
                                .send((
                                    index,
                                    height,
                                    TracesOrInput::CircuitWitness(Box::new(input)),
                                ))
                                .unwrap();

                            // Advance the turn.
                            record_and_trace_sync.advance_turn();
                        } else {
                            break;
                        }
                    }
                });
            }

            // Spawn workers who generate the compress proofs.
            let proofs_sync = Arc::new(TurnBasedSync::new());
            let (proofs_tx, proofs_rx) =
                sync_channel::<(usize, usize, StarkVerifyingKey<InnerSC>, ShardProof<InnerSC>)>(
                    num_first_layer_inputs * 2,
                );
            let proofs_tx = Arc::new(Mutex::new(proofs_tx));
            let proofs_rx = Arc::new(Mutex::new(proofs_rx));
            let mut prover_handles = Vec::new();
            for _ in 0..opts.recursion_opts.shard_batch_size {
                let prover_sync = Arc::clone(&proofs_sync);
                let record_and_trace_rx = Arc::clone(&record_and_trace_rx);
                let proofs_tx = Arc::clone(&proofs_tx);
                let span = tracing::debug_span!("prove");
                let handle = s.spawn(move || {
                    let _span = span.enter();
                    loop {
                        let received = { record_and_trace_rx.lock().unwrap().recv() };
                        if let Ok((index, height, TracesOrInput::ProgramRecordTraces(boxed_prt))) =
                            received
                        {
                            let (program, record, traces) = *boxed_prt;
                            tracing::debug_span!("batch").in_scope(|| {
                                // Get the keys.
                                let (pk, vk) = tracing::debug_span!("Setup compress program")
                                    .in_scope(|| self.compress_prover.setup(&program));

                                // Observe the proving key.
                                let mut challenger = self.compress_prover.config().challenger();
                                tracing::debug_span!("observe proving key").in_scope(|| {
                                    pk.observe_into(&mut challenger);
                                });

                                #[cfg(feature = "debug")]
                                self.compress_prover.debug_constraints(
                                    &self.compress_prover.pk_to_host(&pk),
                                    vec![record.clone()],
                                    &mut challenger.clone(),
                                );

                                // Commit to the record and traces.
                                let data = tracing::debug_span!("commit")
                                    .in_scope(|| self.compress_prover.commit(&record, traces));

                                // Generate the proof.
                                let proof = tracing::debug_span!("open").in_scope(|| {
                                    self.compress_prover.open(&pk, data, &mut challenger).unwrap()
                                });

                                // Verify the proof.
                                #[cfg(feature = "debug")]
                                self.compress_prover
                                    .machine()
                                    .verify(
                                        &vk,
                                        &sp1_stark::MachineProof {
                                            shard_proofs: vec![proof.clone()],
                                        },
                                        &mut self.compress_prover.config().challenger(),
                                    )
                                    .unwrap();

                                // Wait for our turn to update the state.
                                prover_sync.wait_for_turn(index);

                                // Send the proof.
                                proofs_tx.lock().unwrap().send((index, height, vk, proof)).unwrap();

                                // Advance the turn.
                                prover_sync.advance_turn();
                            });
                        } else if let Ok((
                            index,
                            height,
                            TracesOrInput::CircuitWitness(witness_box),
                        )) = received
                        {
                            let witness = *witness_box;
                            if let SP1CircuitWitness::Compress(inner_witness) = witness {
                                let SP1CompressWitnessValues { vks_and_proofs, is_complete: _ } =
                                    inner_witness;
                                assert!(vks_and_proofs.len() == 1);
                                let (vk, proof) = vks_and_proofs.last().unwrap();
                                // Wait for our turn to update the state.
                                prover_sync.wait_for_turn(index);

                                // Send the proof.
                                proofs_tx
                                    .lock()
                                    .unwrap()
                                    .send((index, height, vk.clone(), proof.clone()))
                                    .unwrap();

                                // Advance the turn.
                                prover_sync.advance_turn();
                            }
                        } else {
                            break;
                        }
                    }
                });
                prover_handles.push(handle);
            }

            // Spawn a worker that generates inputs for the next layer.
            let handle = {
                let input_tx = Arc::clone(&input_tx);
                let proofs_rx = Arc::clone(&proofs_rx);
                let span = tracing::debug_span!("generate next layer inputs");
                s.spawn(move || {
                    let _span = span.enter();
                    let mut count = num_first_layer_inputs;
                    let mut batch: Vec<(
                        usize,
                        usize,
                        StarkVerifyingKey<InnerSC>,
                        ShardProof<InnerSC>,
                    )> = Vec::new();
                    loop {
                        if expected_height == 0 {
                            break;
                        }
                        let received = { proofs_rx.lock().unwrap().recv() };
                        if let Ok((index, height, vk, proof)) = received {
                            batch.push((index, height, vk, proof));

                            // If we haven't reached the batch size, continue.
                            if batch.len() < batch_size {
                                continue;
                            }

                            // Compute whether we're at the last input of a layer.
                            let mut is_last = false;
                            if let Some(first) = batch.first() {
                                is_last = first.1 != height;
                            }

                            // If we're at the last input of a layer, we need to only include the
                            // first input, otherwise we include all inputs.
                            let inputs =
                                if is_last { vec![batch[0].clone()] } else { batch.clone() };

                            let next_input_height = inputs[0].1 + 1;

                            let is_complete = next_input_height == expected_height;

                            let vks_and_proofs = inputs
                                .into_iter()
                                .map(|(_, _, vk, proof)| (vk, proof))
                                .collect::<Vec<_>>();
                            let input = SP1CircuitWitness::Compress(SP1CompressWitnessValues {
                                vks_and_proofs,
                                is_complete,
                            });

                            input_sync.wait_for_turn(count);
                            input_tx
                                .lock()
                                .unwrap()
                                .send((count, next_input_height, input, is_last))
                                .unwrap();
                            input_sync.advance_turn();
                            count += 1;

                            // If we're at the root of the tree, stop generating inputs.
                            if is_complete {
                                break;
                            }

                            // If we were at the last input of a layer, we keep everything but the
                            // first input. Otherwise, we empty the batch.
                            if is_last {
                                batch = vec![batch[1].clone()];
                            } else {
                                batch = Vec::new();
                            }
                        } else {
                            break;
                        }
                    }
                })
            };

            // Wait for all the provers to finish.
            drop(input_tx);
            drop(record_and_trace_tx);
            drop(proofs_tx);

            for handle in prover_handles {
                handle.join().unwrap();
            }
            handle.join().unwrap();
            tracing::debug!("joined handles");

            let (_, _, vk, proof) = proofs_rx.lock().unwrap().recv().unwrap();
            (vk, proof)
        });

        Ok(SP1ReduceProof { vk, proof })
    }

    /// Wrap a reduce proof into a STARK proven over a SNARK-friendly field.
    #[instrument(name = "shrink", level = "info", skip_all)]
    pub fn shrink(
        &self,
        reduced_proof: SP1ReduceProof<InnerSC>,
        opts: SP1ProverOpts,
    ) -> Result<SP1ReduceProof<InnerSC>, SP1RecursionProverError> {
        // Make the compress proof.
        let SP1ReduceProof { vk: compressed_vk, proof: compressed_proof } = reduced_proof;
        let input = SP1CompressWitnessValues {
            vks_and_proofs: vec![(compressed_vk.clone(), compressed_proof)],
            is_complete: true,
        };

        let input_with_merkle = self.make_merkle_proofs(input);

        let program =
            self.shrink_program(ShrinkAir::<BabyBear>::shrink_shape(), &input_with_merkle);

        // Run the compress program.
        let mut runtime = RecursionRuntime::<Val<InnerSC>, Challenge<InnerSC>, _>::new(
            program.clone(),
            self.shrink_prover.config().perm.clone(),
        );

        let mut witness_stream = Vec::new();
        Witnessable::<InnerConfig>::write(&input_with_merkle, &mut witness_stream);

        runtime.witness_stream = witness_stream.into();

        runtime.run().map_err(|e| SP1RecursionProverError::RuntimeError(e.to_string()))?;

        runtime.print_stats();
        tracing::debug!("Shrink program executed successfully");

        let (shrink_pk, shrink_vk) =
            tracing::debug_span!("setup shrink").in_scope(|| self.shrink_prover.setup(&program));

        // Prove the compress program.
        let mut compress_challenger = self.shrink_prover.config().challenger();
        let mut compress_proof = self
            .shrink_prover
            .prove(&shrink_pk, vec![runtime.record], &mut compress_challenger, opts.recursion_opts)
            .unwrap();

        Ok(SP1ReduceProof { vk: shrink_vk, proof: compress_proof.shard_proofs.pop().unwrap() })
    }

    /// Wrap a reduce proof into a STARK proven over a SNARK-friendly field.
    #[instrument(name = "wrap_bn254", level = "info", skip_all)]
    pub fn wrap_bn254(
        &self,
        compressed_proof: SP1ReduceProof<InnerSC>,
        opts: SP1ProverOpts,
    ) -> Result<SP1ReduceProof<OuterSC>, SP1RecursionProverError> {
        let SP1ReduceProof { vk: compressed_vk, proof: compressed_proof } = compressed_proof;
        let input = SP1CompressWitnessValues {
            vks_and_proofs: vec![(compressed_vk, compressed_proof)],
            is_complete: true,
        };
        let input_with_vk = self.make_merkle_proofs(input);

        let program = self.wrap_program();

        // Run the compress program.
        let mut runtime = RecursionRuntime::<Val<InnerSC>, Challenge<InnerSC>, _>::new(
            program.clone(),
            self.shrink_prover.config().perm.clone(),
        );

        let mut witness_stream = Vec::new();
        Witnessable::<InnerConfig>::write(&input_with_vk, &mut witness_stream);

        runtime.witness_stream = witness_stream.into();

        runtime.run().map_err(|e| SP1RecursionProverError::RuntimeError(e.to_string()))?;

        runtime.print_stats();
        tracing::debug!("wrap program executed successfully");

        // Setup the wrap program.
        let (wrap_pk, wrap_vk) =
            tracing::debug_span!("setup wrap").in_scope(|| self.wrap_prover.setup(&program));

        if self.wrap_vk.set(wrap_vk.clone()).is_ok() {
            tracing::debug!("wrap verifier key set");
        }

        // Prove the wrap program.
        let mut wrap_challenger = self.wrap_prover.config().challenger();
        let time = std::time::Instant::now();
        let mut wrap_proof = self
            .wrap_prover
            .prove(&wrap_pk, vec![runtime.record], &mut wrap_challenger, opts.recursion_opts)
            .unwrap();
        let elapsed = time.elapsed();
        tracing::debug!("wrap proving time: {:?}", elapsed);
        let mut wrap_challenger = self.wrap_prover.config().challenger();
        self.wrap_prover.machine().verify(&wrap_vk, &wrap_proof, &mut wrap_challenger).unwrap();
        tracing::debug!("wrapping successful");

        Ok(SP1ReduceProof { vk: wrap_vk, proof: wrap_proof.shard_proofs.pop().unwrap() })
    }

    /// Wrap the STARK proven over a SNARK-friendly field into a PLONK proof.
    #[instrument(name = "wrap_plonk_bn254", level = "info", skip_all)]
    pub fn wrap_plonk_bn254(
        &self,
        proof: SP1ReduceProof<OuterSC>,
        build_dir: &Path,
    ) -> PlonkBn254Proof {
        let input = SP1CompressWitnessValues {
            vks_and_proofs: vec![(proof.vk.clone(), proof.proof.clone())],
            is_complete: true,
        };
        let vkey_hash = sp1_vkey_digest_bn254(&proof);
        let committed_values_digest = sp1_committed_values_digest_bn254(&proof);

        let mut witness = Witness::default();
        input.write(&mut witness);
        witness.write_committed_values_digest(committed_values_digest);
        witness.write_vkey_hash(vkey_hash);

        let prover = PlonkBn254Prover::new();
        let proof = prover.prove(witness, build_dir.to_path_buf());

        // Verify the proof.
        prover
            .verify(
                &proof,
                &vkey_hash.as_canonical_biguint(),
                &committed_values_digest.as_canonical_biguint(),
                build_dir,
            )
            .unwrap();

        proof
    }

    /// Wrap the STARK proven over a SNARK-friendly field into a Groth16 proof.
    #[instrument(name = "wrap_groth16_bn254", level = "info", skip_all)]
    pub fn wrap_groth16_bn254(
        &self,
        proof: SP1ReduceProof<OuterSC>,
        build_dir: &Path,
    ) -> Groth16Bn254Proof {
        let input = SP1CompressWitnessValues {
            vks_and_proofs: vec![(proof.vk.clone(), proof.proof.clone())],
            is_complete: true,
        };
        let vkey_hash = sp1_vkey_digest_bn254(&proof);
        let committed_values_digest = sp1_committed_values_digest_bn254(&proof);

        let mut witness = Witness::default();
        input.write(&mut witness);
        witness.write_committed_values_digest(committed_values_digest);
        witness.write_vkey_hash(vkey_hash);

        let prover = Groth16Bn254Prover::new();
        let proof = prover.prove(witness, build_dir.to_path_buf());

        // Verify the proof.
        prover
            .verify(
                &proof,
                &vkey_hash.as_canonical_biguint(),
                &committed_values_digest.as_canonical_biguint(),
                build_dir,
            )
            .unwrap();

        proof
    }

    pub fn recursion_program(
        &self,
        input: &SP1RecursionWitnessValues<CoreSC>,
    ) -> Arc<RecursionProgram<BabyBear>> {
        // Check if the program is in the cache.
        let mut cache = self.lift_programs_lru.lock().unwrap_or_else(|e| e.into_inner());
        let shape = input.shape();
        let program = cache.get(&shape).cloned();
        drop(cache);
        match program {
            Some(program) => program,
            None => {
                let misses = self.lift_cache_misses.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("core cache miss, misses: {}", misses);
                // Get the operations.
                let builder_span = tracing::debug_span!("build recursion program").entered();
                let mut builder = Builder::<InnerConfig>::default();

                let input =
                    tracing::debug_span!("read input").in_scope(|| input.read(&mut builder));
                tracing::debug_span!("verify").in_scope(|| {
                    SP1RecursiveVerifier::verify(&mut builder, self.core_prover.machine(), input)
                });
                let block =
                    tracing::debug_span!("build block").in_scope(|| builder.into_root_block());
                builder_span.exit();
                // SAFETY: The circuit is well-formed. It does not use synchronization primitives
                // (or possibly other means) to violate the invariants.
                let dsl_program = unsafe { DslIrProgram::new_unchecked(block) };

                // Compile the program.
                let compiler_span = tracing::debug_span!("compile recursion program").entered();
                let mut compiler = AsmCompiler::<InnerConfig>::default();
                let mut program = compiler.compile(dsl_program);
                if let Some(inn_recursion_shape_config) = &self.compress_shape_config {
                    inn_recursion_shape_config.fix_shape(&mut program);
                }
                let program = Arc::new(program);
                compiler_span.exit();

                // Insert the program into the cache.
                let mut cache = self.lift_programs_lru.lock().unwrap_or_else(|e| e.into_inner());
                cache.put(shape, program.clone());
                drop(cache);
                program
            }
        }
    }

    pub fn compress_program(
        &self,
        input: &SP1CompressWithVKeyWitnessValues<InnerSC>,
    ) -> Arc<RecursionProgram<BabyBear>> {
        self.join_programs_map.get(&input.shape()).cloned().unwrap_or_else(|| {
            tracing::warn!("join program not found in map, recomputing join program.");
            // Get the operations.
            Arc::new(compress_program_from_input::<C>(
                self.compress_shape_config.as_ref(),
                &self.compress_prover,
                self.vk_verification,
                input,
            ))
        })
    }

    pub fn shrink_program(
        &self,
        shrink_shape: RecursionShape,
        input: &SP1CompressWithVKeyWitnessValues<InnerSC>,
    ) -> Arc<RecursionProgram<BabyBear>> {
        // Get the operations.
        let builder_span = tracing::debug_span!("build shrink program").entered();
        let mut builder = Builder::<InnerConfig>::default();
        let input = input.read(&mut builder);
        // Verify the proof.
        SP1CompressRootVerifierWithVKey::verify(
            &mut builder,
            self.compress_prover.machine(),
            input,
            self.vk_verification,
            PublicValuesOutputDigest::Reduce,
        );
        let block = builder.into_root_block();
        builder_span.exit();
        // SAFETY: The circuit is well-formed. It does not use synchronization primitives
        // (or possibly other means) to violate the invariants.
        let dsl_program = unsafe { DslIrProgram::new_unchecked(block) };

        // Compile the program.
        let compiler_span = tracing::debug_span!("compile shrink program").entered();
        let mut compiler = AsmCompiler::<InnerConfig>::default();
        let mut program = compiler.compile(dsl_program);

        *program.shape_mut() = Some(shrink_shape);
        let program = Arc::new(program);
        compiler_span.exit();
        program
    }

    pub fn wrap_program(&self) -> Arc<RecursionProgram<BabyBear>> {
        self.wrap_program
            .get_or_init(|| {
                // Get the operations.
                let builder_span = tracing::debug_span!("build compress program").entered();
                let mut builder = Builder::<WrapConfig>::default();

                let shrink_shape: OrderedShape = ShrinkAir::<BabyBear>::shrink_shape().into();
                let input_shape = SP1CompressShape::from(vec![shrink_shape]);
                let shape = SP1CompressWithVkeyShape {
                    compress_shape: input_shape,
                    merkle_tree_height: self.recursion_vk_tree.height,
                };
                let dummy_input =
                    SP1CompressWithVKeyWitnessValues::dummy(self.shrink_prover.machine(), &shape);

                let input = dummy_input.read(&mut builder);

                // Attest that the merkle tree root is correct.
                let root = input.merkle_var.root;
                for (val, expected) in root.iter().zip(self.recursion_vk_root.iter()) {
                    builder.assert_felt_eq(*val, *expected);
                }
                // Verify the proof.
                SP1CompressRootVerifierWithVKey::verify(
                    &mut builder,
                    self.shrink_prover.machine(),
                    input,
                    self.vk_verification,
                    PublicValuesOutputDigest::Root,
                );

                let block = builder.into_root_block();
                builder_span.exit();
                // SAFETY: The circuit is well-formed. It does not use synchronization primitives
                // (or possibly other means) to violate the invariants.
                let dsl_program = unsafe { DslIrProgram::new_unchecked(block) };

                // Compile the program.
                let compiler_span = tracing::debug_span!("compile compress program").entered();
                let mut compiler = AsmCompiler::<WrapConfig>::default();
                let program = Arc::new(compiler.compile(dsl_program));
                compiler_span.exit();
                program
            })
            .clone()
    }

    pub fn deferred_program(
        &self,
        input: &SP1DeferredWitnessValues<InnerSC>,
    ) -> Arc<RecursionProgram<BabyBear>> {
        // Compile the program.

        // Get the operations.
        let operations_span =
            tracing::debug_span!("get operations for the deferred program").entered();
        let mut builder = Builder::<InnerConfig>::default();
        let input_read_span = tracing::debug_span!("Read input values").entered();
        let input = input.read(&mut builder);
        input_read_span.exit();
        let verify_span = tracing::debug_span!("Verify deferred program").entered();

        // Verify the proof.
        SP1DeferredVerifier::verify(
            &mut builder,
            self.compress_prover.machine(),
            input,
            self.vk_verification,
        );
        verify_span.exit();
        let block = builder.into_root_block();
        operations_span.exit();
        // SAFETY: The circuit is well-formed. It does not use synchronization primitives
        // (or possibly other means) to violate the invariants.
        let dsl_program = unsafe { DslIrProgram::new_unchecked(block) };

        let compiler_span = tracing::debug_span!("compile deferred program").entered();
        let mut compiler = AsmCompiler::<InnerConfig>::default();
        let mut program = compiler.compile(dsl_program);
        if let Some(recursion_shape_config) = &self.compress_shape_config {
            recursion_shape_config.fix_shape(&mut program);
        }
        let program = Arc::new(program);
        compiler_span.exit();
        program
    }

    pub fn get_recursion_core_inputs(
        &self,
        vk: &StarkVerifyingKey<CoreSC>,
        shard_proofs: &[ShardProof<CoreSC>],
        batch_size: usize,
        is_complete: bool,
        deferred_digest: [Val<CoreSC>; 8],
    ) -> Vec<SP1RecursionWitnessValues<CoreSC>> {
        let mut core_inputs = Vec::new();

        // Prepare the inputs for the recursion programs.
        for (batch_idx, batch) in shard_proofs.chunks(batch_size).enumerate() {
            let proofs = batch.to_vec();

            core_inputs.push(SP1RecursionWitnessValues {
                vk: vk.clone(),
                shard_proofs: proofs.clone(),
                is_complete,
                is_first_shard: batch_idx == 0,
                vk_root: self.recursion_vk_root,
                reconstruct_deferred_digest: deferred_digest,
            });
        }
        core_inputs
    }

    pub fn get_recursion_deferred_inputs_with_initial_digest<'a>(
        &'a self,
        vk: &'a StarkVerifyingKey<CoreSC>,
        deferred_proofs: &[SP1ReduceProof<InnerSC>],
        mut deferred_digest: [Val<CoreSC>; 8],
        batch_size: usize,
    ) -> (Vec<SP1DeferredWitnessValues<InnerSC>>, [BabyBear; 8]) {
        // Prepare the inputs for the deferred proofs recursive verification.
        let mut deferred_inputs = Vec::new();

        for batch in deferred_proofs.chunks(batch_size) {
            let vks_and_proofs =
                batch.iter().cloned().map(|proof| (proof.vk, proof.proof)).collect::<Vec<_>>();

            let input = SP1CompressWitnessValues { vks_and_proofs, is_complete: true };
            let input = self.make_merkle_proofs(input);
            let SP1CompressWithVKeyWitnessValues { compress_val, merkle_val } = input;

            deferred_inputs.push(SP1DeferredWitnessValues {
                vks_and_proofs: compress_val.vks_and_proofs,
                vk_merkle_data: merkle_val,
                start_reconstruct_deferred_digest: deferred_digest,
                is_complete: false,
                sp1_vk_digest: vk.hash_babybear(),
                end_pc: vk.pc_start,
                end_shard: BabyBear::one(),
                end_execution_shard: BabyBear::one(),
                init_addr_bits: [BabyBear::zero(); 32],
                finalize_addr_bits: [BabyBear::zero(); 32],
                committed_value_digest: [Word::<BabyBear>([BabyBear::zero(); 4]); 8],
                deferred_proofs_digest: [BabyBear::zero(); 8],
            });

            deferred_digest = Self::hash_deferred_proofs(deferred_digest, batch);
        }
        (deferred_inputs, deferred_digest)
    }

    pub fn get_recursion_deferred_inputs<'a>(
        &'a self,
        vk: &'a StarkVerifyingKey<CoreSC>,
        deferred_proofs: &[SP1ReduceProof<InnerSC>],
        batch_size: usize,
    ) -> (Vec<SP1DeferredWitnessValues<InnerSC>>, [BabyBear; 8]) {
        self.get_recursion_deferred_inputs_with_initial_digest(
            vk,
            deferred_proofs,
            [Val::<CoreSC>::zero(); DIGEST_SIZE],
            batch_size,
        )
    }

    /// Generate the inputs for the first layer of recursive proofs.
    #[allow(clippy::type_complexity)]
    pub fn get_first_layer_inputs<'a>(
        &'a self,
        vk: &'a SP1VerifyingKey,
        shard_proofs: &[ShardProof<InnerSC>],
        deferred_proofs: &[SP1ReduceProof<InnerSC>],
        batch_size: usize,
    ) -> Vec<SP1CircuitWitness> {
        let (deferred_inputs, deferred_digest) =
            self.get_recursion_deferred_inputs(&vk.vk, deferred_proofs, batch_size);

        let is_complete = shard_proofs.len() == 1 && deferred_proofs.is_empty();
        let core_inputs = self.get_recursion_core_inputs(
            &vk.vk,
            shard_proofs,
            batch_size,
            is_complete,
            deferred_digest,
        );

        let mut inputs = Vec::new();
        inputs.extend(deferred_inputs.into_iter().map(SP1CircuitWitness::Deferred));
        inputs.extend(core_inputs.into_iter().map(SP1CircuitWitness::Core));
        inputs
    }

    /// Accumulate deferred proofs into a single digest.
    pub fn hash_deferred_proofs(
        prev_digest: [Val<CoreSC>; DIGEST_SIZE],
        deferred_proofs: &[SP1ReduceProof<InnerSC>],
    ) -> [Val<CoreSC>; 8] {
        let mut digest = prev_digest;
        for proof in deferred_proofs.iter() {
            let pv: &RecursionPublicValues<Val<CoreSC>> =
                proof.proof.public_values.as_slice().borrow();
            let committed_values_digest = words_to_bytes(&pv.committed_value_digest);
            digest = hash_deferred_proof(
                &digest,
                &pv.sp1_vk_digest,
                &committed_values_digest.try_into().unwrap(),
            );
        }
        digest
    }

    pub fn make_merkle_proofs(
        &self,
        input: SP1CompressWitnessValues<CoreSC>,
    ) -> SP1CompressWithVKeyWitnessValues<CoreSC> {
        let num_vks = self.recursion_vk_map.len();
        let (vk_indices, vk_digest_values): (Vec<_>, Vec<_>) = if self.vk_verification {
            input
                .vks_and_proofs
                .iter()
                .map(|(vk, _)| {
                    let vk_digest = vk.hash_babybear();
                    let index = self.recursion_vk_map.get(&vk_digest).expect("vk not allowed");
                    (index, vk_digest)
                })
                .unzip()
        } else {
            input
                .vks_and_proofs
                .iter()
                .map(|(vk, _)| {
                    let vk_digest = vk.hash_babybear();
                    let index = (vk_digest[0].as_canonical_u32() as usize) % num_vks;
                    (index, [BabyBear::from_canonical_usize(index); 8])
                })
                .unzip()
        };

        let proofs = vk_indices
            .iter()
            .map(|index| {
                let (_, proof) = MerkleTree::open(&self.recursion_vk_tree, *index);
                proof
            })
            .collect();

        let merkle_val = SP1MerkleProofWitnessValues {
            root: self.recursion_vk_root,
            values: vk_digest_values,
            vk_merkle_proofs: proofs,
        };

        SP1CompressWithVKeyWitnessValues { compress_val: input, merkle_val }
    }

    fn check_for_high_cycles(cycles: u64) {
        if cycles > 100_000_000 {
            tracing::warn!(
                "High cycle count detected ({}M cycles). For better performance, consider using the Succinct Prover Network: https://docs.succinct.xyz/docs/sp1/generating-proofs/prover-network",
                cycles / 1_000_000
            );
        }
    }
}

pub fn compress_program_from_input<C: SP1ProverComponents>(
    config: Option<&RecursionShapeConfig<BabyBear, CompressAir<BabyBear>>>,
    compress_prover: &C::CompressProver,
    vk_verification: bool,
    input: &SP1CompressWithVKeyWitnessValues<BabyBearPoseidon2>,
) -> RecursionProgram<BabyBear> {
    let builder_span = tracing::debug_span!("build compress program").entered();
    let mut builder = Builder::<InnerConfig>::default();
    // read the input.
    let input = input.read(&mut builder);
    // Verify the proof.
    SP1CompressWithVKeyVerifier::verify(
        &mut builder,
        compress_prover.machine(),
        input,
        vk_verification,
        PublicValuesOutputDigest::Reduce,
    );
    let block = builder.into_root_block();
    builder_span.exit();
    // SAFETY: The circuit is well-formed. It does not use synchronization primitives
    // (or possibly other means) to violate the invariants.
    let dsl_program = unsafe { DslIrProgram::new_unchecked(block) };

    // Compile the program.
    let compiler_span = tracing::debug_span!("compile compress program").entered();
    let mut compiler = AsmCompiler::<InnerConfig>::default();
    let mut program = compiler.compile(dsl_program);
    if let Some(config) = config {
        config.fix_shape(&mut program);
    }
    compiler_span.exit();

    program
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::print_stdout)]

    use std::{
        collections::BTreeSet,
        fs::File,
        io::{Read, Write},
    };

    use super::*;

    use crate::build::try_build_plonk_bn254_artifacts_dev;
    use anyhow::Result;
    use build::{build_constraints_and_witness, try_build_groth16_bn254_artifacts_dev};
    use p3_field::PrimeField32;

    use shapes::SP1ProofShape;
    use sp1_recursion_core::air::RecursionPublicValues;

    #[cfg(test)]
    use serial_test::serial;
    #[cfg(test)]
    use sp1_core_machine::utils::setup_logger;
    use utils::sp1_vkey_digest_babybear;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Test {
        Core,
        Compress,
        Shrink,
        Wrap,
        CircuitTest,
        All,
    }

    pub fn test_e2e_prover<C: SP1ProverComponents>(
        prover: &SP1Prover<C>,
        elf: &[u8],
        stdin: SP1Stdin,
        opts: SP1ProverOpts,
        test_kind: Test,
    ) -> Result<()> {
        run_e2e_prover_with_options(prover, elf, stdin, opts, test_kind, true)
    }

    pub fn bench_e2e_prover<C: SP1ProverComponents>(
        prover: &SP1Prover<C>,
        elf: &[u8],
        stdin: SP1Stdin,
        opts: SP1ProverOpts,
        test_kind: Test,
    ) -> Result<()> {
        run_e2e_prover_with_options(prover, elf, stdin, opts, test_kind, false)
    }

    pub fn run_e2e_prover_with_options<C: SP1ProverComponents>(
        prover: &SP1Prover<C>,
        elf: &[u8],
        stdin: SP1Stdin,
        opts: SP1ProverOpts,
        test_kind: Test,
        verify: bool,
    ) -> Result<()> {
        tracing::info!("initializing prover");
        let context = SP1Context::default();

        tracing::info!("setup elf");
        let (_, pk_d, program, vk) = prover.setup(elf);

        tracing::info!("prove core");
        let core_proof = prover.prove_core(&pk_d, program, &stdin, opts, context)?;
        let public_values = core_proof.public_values.clone();

        if env::var("COLLECT_SHAPES").is_ok() {
            let mut shapes = BTreeSet::new();
            for proof in core_proof.proof.0.iter() {
                let shape = SP1ProofShape::Recursion(proof.shape());
                shapes.insert(shape);
            }

            let mut file = File::create("../shapes.bin").unwrap();
            bincode::serialize_into(&mut file, &shapes).unwrap();
        }

        if verify {
            tracing::info!("verify core");
            prover.verify(&core_proof.proof, &vk)?;
        }

        if test_kind == Test::Core {
            return Ok(());
        }

        tracing::info!("compress");
        let compress_span = tracing::debug_span!("compress").entered();
        let compressed_proof = prover.compress(&vk, core_proof, vec![], opts)?;
        compress_span.exit();

        if verify {
            tracing::info!("verify compressed");
            prover.verify_compressed(&compressed_proof, &vk)?;
        }

        if test_kind == Test::Compress {
            return Ok(());
        }

        tracing::info!("shrink");
        let shrink_proof = prover.shrink(compressed_proof, opts)?;

        if verify {
            tracing::info!("verify shrink");
            prover.verify_shrink(&shrink_proof, &vk)?;
        }

        if test_kind == Test::Shrink {
            return Ok(());
        }

        tracing::info!("wrap bn254");
        let wrapped_bn254_proof = prover.wrap_bn254(shrink_proof, opts)?;
        let bytes = bincode::serialize(&wrapped_bn254_proof).unwrap();

        // Save the proof.
        let mut file = File::create("proof-with-pis.bin").unwrap();
        file.write_all(bytes.as_slice()).unwrap();

        // Load the proof.
        let mut file = File::open("proof-with-pis.bin").unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();

        let wrapped_bn254_proof = bincode::deserialize(&bytes).unwrap();

        if verify {
            tracing::info!("verify wrap bn254");
            prover.verify_wrap_bn254(&wrapped_bn254_proof, &vk).unwrap();
        }

        if test_kind == Test::Wrap {
            return Ok(());
        }

        tracing::info!("checking vkey hash babybear");
        let vk_digest_babybear = sp1_vkey_digest_babybear(&wrapped_bn254_proof);
        assert_eq!(vk_digest_babybear, vk.hash_babybear());

        tracing::info!("checking vkey hash bn254");
        let vk_digest_bn254 = sp1_vkey_digest_bn254(&wrapped_bn254_proof);
        assert_eq!(vk_digest_bn254, vk.hash_bn254());

        tracing::info!("Test the outer Plonk circuit");
        let (constraints, witness) =
            build_constraints_and_witness(&wrapped_bn254_proof.vk, &wrapped_bn254_proof.proof);
        PlonkBn254Prover::test(constraints, witness);
        tracing::info!("Circuit test succeeded");

        if test_kind == Test::CircuitTest {
            return Ok(());
        }

        tracing::info!("generate plonk bn254 proof");
        let artifacts_dir = try_build_plonk_bn254_artifacts_dev(
            &wrapped_bn254_proof.vk,
            &wrapped_bn254_proof.proof,
        );
        let plonk_bn254_proof =
            prover.wrap_plonk_bn254(wrapped_bn254_proof.clone(), &artifacts_dir);
        println!("{plonk_bn254_proof:?}");

        prover.verify_plonk_bn254(&plonk_bn254_proof, &vk, &public_values, &artifacts_dir)?;

        tracing::info!("generate groth16 bn254 proof");
        let artifacts_dir = try_build_groth16_bn254_artifacts_dev(
            &wrapped_bn254_proof.vk,
            &wrapped_bn254_proof.proof,
        );
        let groth16_bn254_proof = prover.wrap_groth16_bn254(wrapped_bn254_proof, &artifacts_dir);
        println!("{groth16_bn254_proof:?}");

        if verify {
            prover.verify_groth16_bn254(
                &groth16_bn254_proof,
                &vk,
                &public_values,
                &artifacts_dir,
            )?;
        }

        Ok(())
    }

    pub fn test_e2e_with_deferred_proofs_prover<C: SP1ProverComponents>(
        opts: SP1ProverOpts,
    ) -> Result<()> {
        // Test program which proves the Keccak-256 hash of various inputs.
        let keccak_elf = test_artifacts::KECCAK256_ELF;

        // Test program which verifies proofs of a vkey and a list of committed inputs.
        let verify_elf = test_artifacts::VERIFY_PROOF_ELF;

        tracing::info!("initializing prover");
        let prover = SP1Prover::<C>::new();

        tracing::info!("setup keccak elf");
        let (_, keccak_pk_d, keccak_program, keccak_vk) = prover.setup(keccak_elf);

        tracing::info!("setup verify elf");
        let (_, verify_pk_d, verify_program, verify_vk) = prover.setup(verify_elf);

        tracing::info!("prove subproof 1");
        let mut stdin = SP1Stdin::new();
        stdin.write(&1usize);
        stdin.write(&vec![0u8, 0, 0]);
        let deferred_proof_1 = prover.prove_core(
            &keccak_pk_d,
            keccak_program.clone(),
            &stdin,
            opts,
            Default::default(),
        )?;
        let pv_1 = deferred_proof_1.public_values.as_slice().to_vec().clone();

        // Generate a second proof of keccak of various inputs.
        tracing::info!("prove subproof 2");
        let mut stdin = SP1Stdin::new();
        stdin.write(&3usize);
        stdin.write(&vec![0u8, 1, 2]);
        stdin.write(&vec![2, 3, 4]);
        stdin.write(&vec![5, 6, 7]);
        let deferred_proof_2 =
            prover.prove_core(&keccak_pk_d, keccak_program, &stdin, opts, Default::default())?;
        let pv_2 = deferred_proof_2.public_values.as_slice().to_vec().clone();

        // Generate recursive proof of first subproof.
        tracing::info!("compress subproof 1");
        let deferred_reduce_1 = prover.compress(&keccak_vk, deferred_proof_1, vec![], opts)?;
        prover.verify_compressed(&deferred_reduce_1, &keccak_vk)?;

        // Generate recursive proof of second subproof.
        tracing::info!("compress subproof 2");
        let deferred_reduce_2 = prover.compress(&keccak_vk, deferred_proof_2, vec![], opts)?;
        prover.verify_compressed(&deferred_reduce_2, &keccak_vk)?;

        // Run verify program with keccak vkey, subproofs, and their committed values.
        let mut stdin = SP1Stdin::new();
        let vkey_digest = keccak_vk.hash_babybear();
        let vkey_digest: [u32; 8] = vkey_digest
            .iter()
            .map(|n| n.as_canonical_u32())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        stdin.write(&vkey_digest);
        stdin.write(&vec![pv_1.clone(), pv_2.clone(), pv_2.clone()]);
        stdin.write_proof(deferred_reduce_1.clone(), keccak_vk.vk.clone());
        stdin.write_proof(deferred_reduce_2.clone(), keccak_vk.vk.clone());
        stdin.write_proof(deferred_reduce_2.clone(), keccak_vk.vk.clone());

        tracing::info!("proving verify program (core)");
        let verify_proof =
            prover.prove_core(&verify_pk_d, verify_program, &stdin, opts, Default::default())?;
        // let public_values = verify_proof.public_values.clone();

        // Generate recursive proof of verify program
        tracing::info!("compress verify program");
        let verify_reduce = prover.compress(
            &verify_vk,
            verify_proof,
            vec![deferred_reduce_1, deferred_reduce_2.clone(), deferred_reduce_2],
            opts,
        )?;
        let reduce_pv: &RecursionPublicValues<_> =
            verify_reduce.proof.public_values.as_slice().borrow();
        println!("deferred_hash: {:?}", reduce_pv.deferred_proofs_digest);
        println!("complete: {:?}", reduce_pv.is_complete);

        tracing::info!("verify verify program");
        prover.verify_compressed(&verify_reduce, &verify_vk)?;

        let shrink_proof = prover.shrink(verify_reduce, opts)?;

        tracing::info!("verify shrink");
        prover.verify_shrink(&shrink_proof, &verify_vk)?;

        tracing::info!("wrap bn254");
        let wrapped_bn254_proof = prover.wrap_bn254(shrink_proof, opts)?;

        tracing::info!("verify wrap bn254");
        println!("verify wrap bn254 {:#?}", wrapped_bn254_proof.vk.commit);
        prover.verify_wrap_bn254(&wrapped_bn254_proof, &verify_vk).unwrap();

        Ok(())
    }

    /// Tests an end-to-end workflow of proving a program across the entire proof generation
    /// pipeline.
    ///
    /// Add `FRI_QUERIES`=1 to your environment for faster execution. Should only take a few minutes
    /// on a Mac M2. Note: This test always re-builds the plonk bn254 artifacts, so setting SP1_DEV
    /// is not needed.
    #[test]
    #[serial]
    fn test_e2e() -> Result<()> {
        let elf = test_artifacts::FIBONACCI_ELF;
        setup_logger();
        let opts = SP1ProverOpts::auto();
        // TODO(mattstam): We should Test::Plonk here, but this uses the existing
        // docker image which has a different API than the current. So we need to wait until the
        // next release (v1.2.0+), and then switch it back.
        let prover = SP1Prover::<CpuProverComponents>::new();
        test_e2e_prover::<CpuProverComponents>(&prover, elf, SP1Stdin::default(), opts, Test::All)
    }

    /// Tests an end-to-end workflow of proving a program across the entire proof generation
    /// pipeline in addition to verifying deferred proofs.
    #[test]
    #[serial]
    fn test_e2e_with_deferred_proofs() -> Result<()> {
        setup_logger();
        test_e2e_with_deferred_proofs_prover::<CpuProverComponents>(SP1ProverOpts::auto())
    }
}
