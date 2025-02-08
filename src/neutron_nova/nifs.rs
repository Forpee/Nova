//! This module implements the non-interactive folding scheme specified in the NeutronNova paper.
//!
//! R1CS folding with ZeroFold from the NeutronNova paper.

use super::{
  running_instance::{
    NSCInstance, NSCPCInstance, NSCPCWitness, NSCWitness, RunningZFInstance, RunningZFWitness,
    ZCPCInstance, ZCPCWitness,
  },
  sumfold::SumFoldProof,
};
use crate::{
  errors::NovaError,
  neutron_nova::sumfold::{nsc_pc_to_sumfold_inputs, nsc_to_sumfold_inputs, sumfold},
  r1cs::{R1CSInstance, R1CSShape, R1CSWitness},
  spartan::polys::eq::EqPolynomial,
  spartan::{math::Math, polys::power::PowPoly},
  traits::{Engine, TranscriptEngineTrait},
  Commitment, CommitmentKey,
};
use ff::Field;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};

/// A SNARK that holds the proof of a step of an incremental computation
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct NIFS<E>
where
  E: Engine,
{
  comm_e: Commitment<E>,
  sf_proof: SumFoldProof<E>,
  T: E::Scalar,
  T_pc: E::Scalar,
}

impl<E> NIFS<E>
where
  E: Engine,
{
  /// Implement prover for the R1CS NeutronNova folding scheme
  pub fn prove(
    S: &R1CSShape<E>,
    ck: &CommitmentKey<E>,
    U1: &RunningZFInstance<E>,
    W1: &RunningZFWitness<E>,
    U2: &R1CSInstance<E>,
    W2: &R1CSWitness<E>,
  ) -> Result<(Self, (RunningZFInstance<E>, RunningZFWitness<E>)), NovaError> {
    let mut transcript = E::TE::new(b"NeutronNova");
    transcript.absorb(b"U2", U2);

    // Collect the instance & witness in ZC_PC from (U1, W1) and reduce them along with zero-check
    // instance, witness (U2, W2) via the zero-check reduction into an instances, witnesses in NSC,
    // an instance, witness pair in NSC_PC, and a fresh instance, witness in ZC_PC
    let (nsc_U2, nsc_W2, nsc_pc_U2, nsc_pc_W2, new_zc_pc_U, new_zc_pc_W) =
      ZeroCheckReduction::prove(
        ck,
        &mut transcript,
        U1.zc_pc(),
        W1.zc_pc(),
        U2.clone(),
        W2.clone(),
        S.num_cons.log_2(),
      )?;

    // Run sumfold prover
    let g = nsc_to_sumfold_inputs(S, U1.nsc().U(), W1.nsc().W(), W1.nsc().e())?;
    let h = nsc_to_sumfold_inputs(S, nsc_U2.U(), nsc_W2.W(), nsc_W2.e())?;
    let F =
      |az: E::Scalar, bz: E::Scalar, cz: E::Scalar, h1: E::Scalar, h2: E::Scalar| -> E::Scalar {
        (az * bz - cz) * h1 * h2
      };
    let g_pc = nsc_pc_to_sumfold_inputs(W1.nsc_pc().e(), W1.nsc_pc().new_e())?;
    let h_pc = nsc_pc_to_sumfold_inputs(nsc_pc_W2.e(), nsc_pc_W2.new_e())?;
    let F_pc =
      |g1: E::Scalar, g2: E::Scalar, g3: E::Scalar, h1: E::Scalar, h2: E::Scalar| -> E::Scalar {
        (g1 - g2 * g3) * h1 * h2
      };
    let gamma = transcript.squeeze(b"gamma")?;
    let (sf_proof, r_b, T, T_pc) = sumfold(
      &mut transcript,
      &g,
      &h,
      U1.nsc().T(),
      F,
      &g_pc,
      &h_pc,
      U1.nsc_pc().T(),
      F_pc,
      gamma,
    )?;

    // Send T & T_pc to verifier
    transcript.absorb(b"T", &T);
    transcript.absorb(b"T_pc", &T_pc);

    // create NIFS
    let nifs = Self {
      comm_e: new_zc_pc_U.comm_e(),
      sf_proof,
      T,
      T_pc,
    };

    // Output the running zero-fold instance, witness pair
    let U = U1.fold(&nsc_U2, r_b, T, &nsc_pc_U2, T_pc, new_zc_pc_U);
    let W = W1.fold(&nsc_W2, r_b, &nsc_pc_W2, new_zc_pc_W);
    Ok((nifs, (U, W)))
  }

  /// Implement verifier for the R1CS NeutronNova folding scheme
  pub fn verify(
    &self,
    U1: &RunningZFInstance<E>,
    U2: &R1CSInstance<E>,
  ) -> Result<RunningZFInstance<E>, NovaError> {
    let mut transcript = E::TE::new(b"NeutronNova");
    transcript.absorb(b"U2", U2);

    // Collect the instance in ZC_PC from U1 and reduce them along with zero-check
    // instance U2 via the zero-check reduction into an instances in NSC,
    // an instance in NSC_PC, and a fresh instance in ZC_PC
    let (nsc_U2, nsc_pc_U2, new_zc_pc_U) =
      ZeroCheckReduction::verify(&mut transcript, U1.zc_pc(), U2.clone(), self.comm_e)?;

    // Verify the sumfold proof
    let gamma = transcript.squeeze(b"gamma")?;
    let (c, beta, r_b) = self.sf_proof.verify(
      &mut transcript,
      U1.nsc().T() + gamma * U1.nsc_pc.T(),
      E::Scalar::ZERO,
    )?;

    //  Check T_γ = T + γ · T_pc,
    transcript.absorb(b"T", &self.T);
    transcript.absorb(b"T_pc", &self.T_pc);
    let T_gamma = c
      * (EqPolynomial::new(vec![beta])
        .evaluate(&[r_b])
        .invert()
        .unwrap());
    if T_gamma != self.T + gamma * self.T_pc {
      return Err(NovaError::ProofVerifyError);
    }

    // Ouput the running zero-fold instance
    let U = U1.fold(&nsc_U2, r_b, self.T, &nsc_pc_U2, self.T_pc, new_zc_pc_U);
    Ok(U)
  }
}

/// Implements the prover and verifier for the zero-check reduction
struct ZeroCheckReduction;

impl ZeroCheckReduction {
  fn prove<E>(
    ck: &CommitmentKey<E>,
    transcript: &mut E::TE,
    ZC_PC_U: &ZCPCInstance<E>,
    ZC_PC_W: &ZCPCWitness<E>,
    U: R1CSInstance<E>,
    W: R1CSWitness<E>,
    ell: usize,
  ) -> Result<
    (
      NSCInstance<E>,
      NSCWitness<E>,
      NSCPCInstance<E>,
      NSCPCWitness<E>,
      ZCPCInstance<E>,
      ZCPCWitness<E>,
    ),
    NovaError,
  >
  where
    E: Engine,
  {
    let tau = transcript.squeeze(b"tau")?;
    let e = PowPoly::new(tau, ell);
    let r_e = E::Scalar::random(&mut OsRng);
    let comm_e = e.commit::<E>(ck, r_e);
    transcript.absorb(b"comm_e", &comm_e);
    let nsc_U = NSCInstance::new(E::Scalar::ZERO, U, comm_e);
    let nsc_W = NSCWitness::new(W, e.clone(), r_e);
    let nsc_pc_U = NSCPCInstance::new(E::Scalar::ZERO, ZC_PC_U.comm_e(), tau, comm_e);
    let nsc_pc_W = NSCPCWitness::new(ZC_PC_W.e().clone(), e.clone(), ZC_PC_W.r_e(), r_e);
    let new_zc_pc_U = ZCPCInstance::new(comm_e, tau);
    let new_zc_pc_W = ZCPCWitness::new(e, r_e);
    Ok((nsc_U, nsc_W, nsc_pc_U, nsc_pc_W, new_zc_pc_U, new_zc_pc_W))
  }

  fn verify<E>(
    transcript: &mut E::TE,
    ZC_PC_U: &ZCPCInstance<E>,
    U: R1CSInstance<E>,
    comm_e: Commitment<E>,
  ) -> Result<(NSCInstance<E>, NSCPCInstance<E>, ZCPCInstance<E>), NovaError>
  where
    E: Engine,
  {
    let tau = transcript.squeeze(b"tau")?;
    transcript.absorb(b"comm_e", &comm_e);
    let nsc_U = NSCInstance::new(E::Scalar::ZERO, U, comm_e);
    let nsc_pc_U = NSCPCInstance::new(E::Scalar::ZERO, ZC_PC_U.comm_e(), tau, comm_e);
    let new_zc_pc_U = ZCPCInstance::new(comm_e, tau);
    Ok((nsc_U, nsc_pc_U, new_zc_pc_U))
  }
}
