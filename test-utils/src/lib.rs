#![no_std]

#[cfg(feature = "std")]
extern crate std;

#[macro_use]
extern crate alloc;

use alloc::{string::String, vec::Vec};
// IMPORTS
// ================================================================================================
#[cfg(not(target_family = "wasm"))]
use proptest::prelude::{Arbitrary, Strategy};

use vm_core::chiplets::hasher::apply_permutation;

// EXPORTS
// ================================================================================================

pub use assembly::{Library, MaslLibrary};
pub use processor::{
    AdviceInputs, AdviceProvider, ContextId, DefaultHost, ExecutionError, ExecutionOptions,
    ExecutionTrace, Process, ProcessState, StackInputs, VmStateIterator,
};
pub use prover::{prove, MemAdviceProvider, ProvingOptions};
pub use test_case::test_case;
pub use verifier::{verify, AcceptableOptions, ProgramInfo, VerifierError};
pub use vm_core::{
    chiplets::hasher::{hash_elements, STATE_WIDTH},
    stack::STACK_TOP_SIZE,
    utils::{collections, group_slice_elements, group_vector_elements, IntoBytes, ToElements},
    Felt, FieldElement, Program, StarkField, Word, EMPTY_WORD, ONE, WORD_SIZE, ZERO,
};

pub mod math {
    pub use winter_prover::math::{
        fft, fields::QuadExtension, polynom, ExtensionOf, FieldElement, StarkField, ToElements,
    };
}

pub mod serde {
    pub use vm_core::utils::{
        ByteReader, ByteWriter, Deserializable, DeserializationError, Serializable, SliceReader,
    };
}

pub mod crypto;

#[cfg(not(target_family = "wasm"))]
pub mod rand;

mod test_builders;

use assembly::AssemblyError;
#[cfg(not(target_family = "wasm"))]
pub use proptest;

// TYPE ALIASES
// ================================================================================================

pub type QuadFelt = vm_core::QuadExtension<Felt>;

// CONSTANTS
// ================================================================================================

/// A value just over what a [u32] integer can hold.
pub const U32_BOUND: u64 = u32::MAX as u64 + 1;

// TEST HANDLER
// ================================================================================================

/// This is used to specify the expected error type when using Test to test errors.
/// `Test::expect_error` will try to either compile or execute the test data, according to the
/// provided TestError variant. Then it will validate that the resulting error contains the
/// TestError variant's string slice.
#[derive(Debug, PartialEq)]
pub enum TestError {
    AssemblyError(AssemblyError),
    ExecutionError(ExecutionError),
}

/// This is a container for the data required to run tests, which allows for running several
/// different types of tests.
///
/// Types of valid result tests:
/// - Execution test: check that running a program compiled from the given source has the
///   specified results for the given (optional) inputs.
/// - Proptest: run an execution test inside a proptest.
///
/// Types of failure tests:
/// - Assembly error test: check that attempting to compile the given source causes an
///   AssemblyError which contains the specified substring.
/// - Execution error test: check that running a program compiled from the given source causes
///   an ExecutionError which contains the specified substring.
pub struct Test {
    pub source: String,
    pub kernel: Option<String>,
    pub stack_inputs: StackInputs,
    pub advice_inputs: AdviceInputs,
    pub in_debug_mode: bool,
    pub libraries: Vec<MaslLibrary>,
}

impl Test {
    // CONSTRUCTOR
    // --------------------------------------------------------------------------------------------

    /// Creates the simplest possible new test, with only a source string and no inputs.
    pub fn new(source: &str, in_debug_mode: bool) -> Self {
        Test {
            source: String::from(source),
            kernel: None,
            stack_inputs: StackInputs::default(),
            advice_inputs: AdviceInputs::default(),
            in_debug_mode,
            libraries: Vec::default(),
        }
    }

    // TEST METHODS
    // --------------------------------------------------------------------------------------------

    /// Asserts that running the test will result in the expected error.
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    pub fn expect_error(&self, expected_error: TestError) {
        match expected_error {
            TestError::AssemblyError(assembly_error) => {
                let actual_error = self.compile().err().unwrap();
                assert_eq!(assembly_error, actual_error);
            }
            TestError::ExecutionError(execution_error) => {
                let actual_error = self.execute().err().unwrap();
                assert_eq!(execution_error, actual_error);
            }
        };
    }

    /// Builds a final stack from the provided stack-ordered array and asserts that executing the
    /// test will result in the expected final stack state.
    pub fn expect_stack(&self, final_stack: &[u64]) {
        let result = stack_to_ints(&self.get_last_stack_state());
        let expected = stack_top_to_ints(final_stack);
        assert_eq!(expected, result, "Expected stack to be {:?}, found {:?}", expected, result);
    }

    /// Executes the test and validates that the process memory has the elements of `expected_mem`
    /// at address `mem_start_addr` and that the end of the stack execution trace matches the
    /// `final_stack`.
    pub fn expect_stack_and_memory(
        &self,
        final_stack: &[u64],
        mut mem_start_addr: u32,
        expected_mem: &[u64],
    ) {
        // compile the program
        let program = self.compile().expect("Failed to compile test source.");
        let host = DefaultHost::new(MemAdviceProvider::from(self.advice_inputs.clone()));

        // execute the test
        let mut process = Process::new(
            program.kernel().clone(),
            self.stack_inputs.clone(),
            host,
            ExecutionOptions::default(),
        );
        process.execute(&program).unwrap();

        // validate the memory state
        for data in expected_mem.chunks(WORD_SIZE) {
            // Main memory is zeroed by default, use zeros as a fallback when unwrap to make testing easier
            let mem_state =
                process.get_mem_value(ContextId::root(), mem_start_addr).unwrap_or(EMPTY_WORD);

            let mem_state = stack_to_ints(&mem_state);
            assert_eq!(
                data, mem_state,
                "Expected memory [{}] => {:?}, found {:?}",
                mem_start_addr, data, mem_state
            );
            mem_start_addr += 1;
        }

        // validate the stack state
        self.expect_stack(final_stack);
    }

    /// Asserts that executing the test inside a proptest results in the expected final stack state.
    /// The proptest will return a test failure instead of panicking if the assertion condition
    /// fails.
    #[cfg(not(target_family = "wasm"))]
    pub fn prop_expect_stack(
        &self,
        final_stack: &[u64],
    ) -> Result<(), proptest::prelude::TestCaseError> {
        let result = self.get_last_stack_state();
        proptest::prop_assert_eq!(stack_top_to_ints(final_stack), stack_to_ints(&result));

        Ok(())
    }

    // UTILITY METHODS
    // --------------------------------------------------------------------------------------------

    /// Compiles a test's source and returns the resulting Program or Assembly error.
    pub fn compile(&self) -> Result<Program, AssemblyError> {
        let assembler = assembly::Assembler::default()
            .with_debug_mode(self.in_debug_mode)
            .with_libraries(self.libraries.iter())
            .expect("failed to load stdlib");

        match self.kernel.as_ref() {
            Some(kernel) => assembler.with_kernel(kernel).expect("kernel compilation failed"),
            None => assembler,
        }
        .compile(&self.source)
    }

    /// Compiles the test's source to a Program and executes it with the tests inputs. Returns a
    /// resulting execution trace or error.
    pub fn execute(&self) -> Result<ExecutionTrace, ExecutionError> {
        let program = self.compile().expect("Failed to compile test source.");
        let host = DefaultHost::new(MemAdviceProvider::from(self.advice_inputs.clone()));
        processor::execute(&program, self.stack_inputs.clone(), host, ExecutionOptions::default())
    }

    /// Compiles the test's source to a Program and executes it with the tests inputs. Returns the
    /// process once execution is finished.
    pub fn execute_process(
        &self,
    ) -> Result<Process<DefaultHost<MemAdviceProvider>>, ExecutionError> {
        let program = self.compile().expect("Failed to compile test source.");
        let host = DefaultHost::new(MemAdviceProvider::from(self.advice_inputs.clone()));
        let mut process = Process::new(
            program.kernel().clone(),
            self.stack_inputs.clone(),
            host,
            ExecutionOptions::default(),
        );
        process.execute(&program)?;
        Ok(process)
    }

    /// Compiles the test's code into a program, then generates and verifies a proof of execution
    /// using the given public inputs and the specified number of stack outputs. When `test_fail`
    /// is true, this function will force a failure by modifying the first output.
    pub fn prove_and_verify(&self, pub_inputs: Vec<u64>, test_fail: bool) {
        let stack_inputs = StackInputs::try_from_ints(pub_inputs).unwrap();
        let program = self.compile().expect("Failed to compile test source.");
        let host = DefaultHost::new(MemAdviceProvider::from(self.advice_inputs.clone()));
        let (mut stack_outputs, proof) =
            prover::prove(&program, stack_inputs.clone(), host, ProvingOptions::default()).unwrap();

        let program_info = ProgramInfo::from(program);
        if test_fail {
            stack_outputs.stack_mut()[0] += ONE;
            assert!(verifier::verify(program_info, stack_inputs, stack_outputs, proof).is_err());
        } else {
            let result = verifier::verify(program_info, stack_inputs, stack_outputs, proof);
            assert!(result.is_ok(), "error: {result:?}");
        }
    }

    /// Compiles the test's source to a Program and executes it with the tests inputs. Returns a
    /// VmStateIterator that allows us to iterate through each clock cycle and inspect the process
    /// state.
    pub fn execute_iter(&self) -> VmStateIterator {
        let program = self.compile().expect("Failed to compile test source.");
        let host = DefaultHost::new(MemAdviceProvider::from(self.advice_inputs.clone()));
        processor::execute_iter(&program, self.stack_inputs.clone(), host)
    }

    /// Returns the last state of the stack after executing a test.
    pub fn get_last_stack_state(&self) -> [Felt; STACK_TOP_SIZE] {
        let trace = self.execute().unwrap();

        trace.last_stack_state()
    }
}

// HELPER FUNCTIONS
// ================================================================================================

/// Converts an array of Felts into u64
pub fn stack_to_ints(values: &[Felt]) -> Vec<u64> {
    values.iter().map(|e| (*e).as_int()).collect()
}

pub fn stack_top_to_ints(values: &[u64]) -> Vec<u64> {
    let mut result: Vec<u64> = values.to_vec();
    result.resize(STACK_TOP_SIZE, 0);
    result
}

/// A proptest strategy for generating a random word with 4 values of type T.
#[cfg(not(target_family = "wasm"))]
pub fn prop_randw<T: Arbitrary>() -> impl Strategy<Value = Vec<T>> {
    use proptest::prelude::{any, prop};
    prop::collection::vec(any::<T>(), 4)
}

/// Given a hasher state, perform one permutation.
///
/// The values of `values` should be:
/// - 0..4 the capacity
/// - 4..12 the rate
///
/// Return the result of the permutation in stack order.
pub fn build_expected_perm(values: &[u64]) -> [Felt; STATE_WIDTH] {
    let mut expected = [ZERO; STATE_WIDTH];
    for (&value, result) in values.iter().zip(expected.iter_mut()) {
        *result = Felt::new(value);
    }
    apply_permutation(&mut expected);
    expected.reverse();

    expected
}

pub fn build_expected_hash(values: &[u64]) -> [Felt; 4] {
    let digest = hash_elements(&values.iter().map(|&v| Felt::new(v)).collect::<Vec<_>>());
    let mut expected: [Felt; 4] = digest.into();
    expected.reverse();

    expected
}
