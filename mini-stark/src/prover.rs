use crate::challenges::Challenges;
use crate::channel::ProverChannel;
use crate::merkle::MerkleTree;
use crate::utils::Timer;
use crate::Air;
use crate::Constraint;
use crate::Matrix;
use crate::Trace;
use crate::TraceInfo;
use ark_ff::One;
use ark_ff::Zero;
use ark_poly::domain::Radix2EvaluationDomain;
use ark_poly::univariate::DensePolynomial;
use ark_poly::DenseUVPolynomial;
use ark_poly::EvaluationDomain;
use ark_poly::Polynomial;
use ark_serialize::CanonicalDeserialize;
use ark_serialize::CanonicalSerialize;
use fast_poly::allocator::PageAlignedAllocator;
use fast_poly::plan::PLANNER;
use fast_poly::stage::MulPowStage;
use fast_poly::utils::buffer_no_copy;
use fast_poly::GpuField;
use sha2::Sha256;
use std::time::Instant;

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

    /// Return value is of the form `(lde, polys, merkle_tree)`
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
        (trace_lde, trace_polys, merkle_tree)
    }

    /// builds a commitment to the combined constraint quotient evaluations.
    /// Output is of the form `(combined_lde, combined_poly, lde_merkle_tree)`
    fn build_constraint_commitment(
        &self,
        boundary_constraint_evals: Matrix<Self::Fp>,
        transition_constraint_evals: Matrix<Self::Fp>,
        terminal_constraint_evals: Matrix<Self::Fp>,
        air: &Self::Air,
    ) -> (Matrix<Self::Fp>, Matrix<Self::Fp>, MerkleTree<Sha256>) {
        let boundary_divisor = air.boundary_constraint_divisor();
        let terminal_divisor = air.terminal_constraint_divisor();
        let transition_divisor = air.transition_constraint_divisor();

        let all_quotients = Matrix::join(vec![
            self.generate_quotients(boundary_constraint_evals, &boundary_divisor),
            self.generate_quotients(transition_constraint_evals, &transition_divisor),
            self.generate_quotients(terminal_constraint_evals, &terminal_divisor),
        ]);

        let eval_matrix = all_quotients.sum_columns();
        let poly_matrix = eval_matrix.interpolate_columns(air.lde_domain());
        let merkle_tree = eval_matrix.commit_to_rows();

        (eval_matrix, poly_matrix, merkle_tree)
    }

    fn evaluate_constraints(
        &self,
        challenges: &Challenges<Self::Fp>,
        constraints: &[Constraint<Self::Fp>],
        trace_lde: &Matrix<Self::Fp>,
    ) -> Matrix<Self::Fp> {
        let trace_step = self.options().blowup_factor as usize;
        Matrix::join(
            constraints
                .iter()
                .map(|constraint| constraint.evaluate_symbolic(challenges, trace_step, trace_lde))
                .collect(),
        )
    }

    fn generate_quotients(
        &self,
        mut all_evaluations: Matrix<Self::Fp>,
        divisor: &Vec<Self::Fp, PageAlignedAllocator>,
    ) -> Matrix<Self::Fp> {
        let library = &PLANNER.library;
        let command_queue = &PLANNER.command_queue;
        let command_buffer = command_queue.new_command_buffer();
        let multiplier = MulPowStage::<Self::Fp>::new(library, divisor.len(), 0);
        let divisor_buffer = buffer_no_copy(command_queue.device(), divisor);
        // TODO: let's move GPU stuff out of here and make it readable in here.
        for evaluations in &mut all_evaluations.0 {
            let mut evaluations_buffer = buffer_no_copy(command_queue.device(), evaluations);
            multiplier.encode(command_buffer, &mut evaluations_buffer, &divisor_buffer, 0);
        }
        command_buffer.commit();
        command_buffer.wait_until_completed();
        all_evaluations
    }

    fn generate_proof(&self, trace: Self::Trace) -> Result<Proof, ProvingError> {
        let _timer = Timer::new("proof generation");

        let options = self.options();
        let trace_info = trace.info();
        let pub_inputs = self.get_pub_inputs(&trace);
        let air = Self::Air::new(trace_info.clone(), pub_inputs, options);
        let mut channel = ProverChannel::<Self::Air, Sha256>::new(&air);

        {
            let ce_blowup_factor = air.ce_blowup_factor();
            let lde_blowup_factor = air.lde_blowup_factor();
            assert!(ce_blowup_factor <= lde_blowup_factor, "constraint evaluation blowup factor {ce_blowup_factor} is larger than the lde blowup factor {lde_blowup_factor}");
        }

        let (base_trace_lde, base_trace_polys, base_trace_lde_tree) =
            self.build_trace_commitment(trace.base_columns(), air.trace_domain(), air.lde_domain());

        channel.commit_trace(base_trace_lde_tree.root());
        // let num_challenges = 20;
        // TODO:
        let num_challenges = air.num_challenges();
        println!("NUM CHALLENGE: {num_challenges}");
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

        // TODO: expensive. wrap in debug feature
        air.validate(&challenges, &trace_polys.evaluate(air.trace_domain()));

        let boundary_constraint_evals =
            self.evaluate_constraints(&challenges, air.boundary_constraints(), &trace_lde);
        let transition_constraint_evals =
            self.evaluate_constraints(&challenges, air.transition_constraints(), &trace_lde);
        let terminal_constraint_evals =
            self.evaluate_constraints(&challenges, air.terminal_constraints(), &trace_lde);

        let (composition_lde, composition_poly, composition_lde_tree) = self
            .build_constraint_commitment(
                boundary_constraint_evals,
                transition_constraint_evals,
                terminal_constraint_evals,
                &air,
            );

        let poly = DensePolynomial::from_coefficients_vec(composition_poly.0[0].to_vec());
        println!("Poly degree is: {}", poly.degree());

        Ok(Proof {
            options,
            trace_info,
            commitments: Vec::new(),
        })
    }
}
