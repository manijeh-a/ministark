use crate::channel::ProverChannel;
use crate::merkle::MerkleTree;
use crate::utils::Timer;
use crate::Air;
use crate::Matrix;
use crate::Trace;
use crate::TraceInfo;
use ark_poly::domain::Radix2EvaluationDomain;
use ark_serialize::CanonicalDeserialize;
use ark_serialize::CanonicalSerialize;
use fast_poly::allocator::PageAlignedAllocator;
use fast_poly::GpuField;
use sha2::Sha256;

// TODO: include ability to specify:
// - base field
// - extension field
// - hashing function
// - determine if grinding factor is appropriate
// - fri folding factor
// - fri max remainder size
#[derive(Debug, Clone, Copy, CanonicalSerialize, CanonicalDeserialize)]
pub struct ProofOptions {
    pub num_queries: u8,
    // would be nice to make this clear as LDE blowup factor vs constraint blowup factor
    pub blowup_factor: u8,
}

impl ProofOptions {
    pub fn new(num_queries: u8, blowup_factor: u8) -> Self {
        ProofOptions {
            num_queries,
            blowup_factor,
        }
    }
}

/// A proof generated by a mini-stark prover
#[derive(Debug, Clone)]
pub struct Proof {
    options: ProofOptions,
    trace_info: TraceInfo,
    commitments: Vec<u64>,
}

/// Errors that can occur during the proving stage
#[derive(Debug)]
pub enum ProvingError {
    Fail,
    // /// This error occurs when a transition constraint evaluated over a specific execution
    // trace /// does not evaluate to zero at any of the steps.
    // UnsatisfiedTransitionConstraintError(usize),
    // /// This error occurs when polynomials built from the columns of a constraint evaluation
    // /// table do not all have the same degree.
    // MismatchedConstraintPolynomialDegree(usize, usize),
}

pub trait Prover {
    type Fp: GpuField;
    type Air: Air<Fp = Self::Fp>;
    type Trace: Trace<Fp = Self::Fp>;

    fn new(options: ProofOptions) -> Self;

    fn get_pub_inputs(&self, trace: &Self::Trace) -> <Self::Air as Air>::PublicInputs;

    fn options(&self) -> ProofOptions;

    /// Return value is of the form `(low_degree_extension, polynomials,
    /// merkle_tree)`
    fn build_trace_commitment(
        &self,
        trace: &Matrix<Self::Fp>,
        trace_domain: Radix2EvaluationDomain<Self::Fp>,
        lde_domain: Radix2EvaluationDomain<Self::Fp>,
    ) -> (Matrix<Self::Fp>, Matrix<Self::Fp>, MerkleTree<Sha256>) {
        let trace_polys = {
            let _timer = Timer::new("trace interpolation");
            trace.interpolate_columns(trace_domain)
        };
        let trace_lde = {
            let _timer = Timer::new("trace low degree extension");
            trace_polys.evaluate(lde_domain)
        };
        let merkle_tree = {
            let _timer = Timer::new("trace commitment");
            trace_lde.commit_to_rows()
        };
        (trace_polys, trace_lde, merkle_tree)
    }

    fn generate_proof(&self, trace: Self::Trace) -> Result<Proof, ProvingError> {
        let _timer = Timer::new("proof generation");

        let options = self.options();
        let trace_info = trace.info();
        let pub_inputs = self.get_pub_inputs(&trace);
        let air = Self::Air::new(trace_info.clone(), pub_inputs, options);
        let mut channel = ProverChannel::<Self::Air, Sha256>::new(&air);

        {
            assert!(
                air.transition_constraints()
                    .into_iter()
                    .map(|constraint| constraint.degree())
                    .max()
                    .unwrap_or_default()
                    <= options.blowup_factor.into()
            );
        }

        let (base_trace_lde, base_trace_polys, base_trace_lde_tree) =
            self.build_trace_commitment(trace.base_columns(), air.trace_domain(), air.lde_domain());

        channel.commit_trace(base_trace_lde_tree.root());
        let num_challenges = 20;
        // TODO:
        // let num_challenges = air.num_challenges();
        let challenges = channel.get_challenges::<Self::Fp>(num_challenges);

        let mut trace_lde = base_trace_lde;
        let mut trace_polys = base_trace_polys;
        let mut extension_trace_tree = None;

        if let Some(extension_matrix) = trace.build_extension_columns(&challenges) {
            let (extension_lde, extension_polys, extension_lde_tree) = self.build_trace_commitment(
                &extension_matrix,
                air.trace_domain(),
                air.lde_domain(),
            );
            channel.commit_trace(extension_lde_tree.root());
            // TODO: this approach could be better
            extension_trace_tree = Some(extension_lde_tree);
            trace_lde.append(extension_lde);
            trace_polys.append(extension_polys);
        }

        let lde_step = air.lde_blowup_factor();
        {
            let _timer = Timer::new("Constraint evaluations");
            let constraint_evaluations = air
                .transition_constraints()
                .iter()
                .map(|constraint| constraint.evaluate_symbolic(&challenges, lde_step, &trace_lde))
                .collect::<Vec<Vec<Self::Fp, PageAlignedAllocator>>>();
        }

        Ok(Proof {
            options,
            trace_info,
            commitments: Vec::new(),
        })
    }
}
