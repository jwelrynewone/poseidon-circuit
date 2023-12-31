#![allow(dead_code)]
use std::{iter, marker::PhantomData, mem};

use halo2_proofs::arithmetic::CurveAffine;
use halo2curves::group::ff::{FromUniformBytes, PrimeField};
use poseidon::{SparseMDSMatrix, Spec};

use crate::ro_types::{ROConstantsTrait, ROTrait};

// adapted from: https://github.com/privacy-scaling-explorations/snark-verifier

#[derive(Clone, Debug)]
struct State<F: PrimeField + FromUniformBytes<64>, const T: usize, const RATE: usize> {
    inner: [F; T],
}

impl<F: PrimeField + FromUniformBytes<64>, const T: usize, const RATE: usize> State<F, T, RATE> {
    fn new(inner: [F; T]) -> Self {
        Self { inner }
    }

    fn sbox_full(&mut self, constants: &[F; T]) {
        let pow5 = |v: &F| v.square() * v.square() * v;
        for (state, constant) in self.inner.iter_mut().zip(constants.iter()) {
            *state = pow5(state) + *constant;
        }
    }

    fn sbox_part(&mut self, constant: &F) {
        let pow5 = |v: &F| v.square() * v.square() * v;
        self.inner[0] = pow5(&self.inner[0]) + *constant;
    }

    fn pre_round(&mut self, inputs: &[F], pre_constants: &[F; T]) {
        assert!(RATE == T - 1);
        assert!(inputs.len() <= RATE);

        self.inner[0] += pre_constants[0];
        self.inner
            .iter_mut()
            .zip(pre_constants.iter())
            .skip(1)
            .zip(inputs)
            .for_each(|((state, constant), input)| {
                *state = *state + *input + *constant;
            });
        self.inner
            .iter_mut()
            .zip(pre_constants.iter())
            .skip(1 + inputs.len())
            .enumerate()
            .for_each(|(idx, (state, constant))| {
                *state = if idx == 0 {
                    *state + F::ONE + *constant
                } else {
                    *state + *constant
                };
            });
    }

    fn apply_mds(&mut self, mds: &[[F; T]; T]) {
        self.inner = mds
            .iter()
            .map(|row| {
                row.iter()
                    .clone()
                    .zip(self.inner.iter())
                    .fold(F::ZERO, |acc, (mij, sj)| acc + *sj * *mij)
            })
            .collect::<Vec<F>>()
            .try_into()
            .unwrap();
    }

    fn apply_sparse_mds(&mut self, mds: &SparseMDSMatrix<F, T, RATE>) {
        self.inner = iter::once(
            mds.row()
                .iter()
                .cloned()
                .zip(self.inner.iter())
                .fold(F::ZERO, |acc, (vi, si)| acc + vi * si),
        )
        .chain(
            mds.col_hat()
                .iter()
                .zip(self.inner.iter().skip(1))
                .map(|(coeff, state)| *coeff * self.inner[0] + *state),
        )
        .collect::<Vec<F>>()
        .try_into()
        .unwrap();
    }
}

impl<F, const T: usize, const RATE: usize> ROConstantsTrait for Spec<F, T, RATE>
where
    F: PrimeField + FromUniformBytes<64>,
{
    fn new(r_f: usize, r_p: usize) -> Self {
        Spec::new(r_f, r_p)
    }
}

impl<C, F, const T: usize, const RATE: usize> ROTrait<C> for PoseidonHash<C, F, T, RATE>
where
    C: CurveAffine<ScalarExt = F>,
    F: PrimeField + FromUniformBytes<64>,
{
    type Constants = Spec<F, T, RATE>;
    fn new(constants: Self::Constants) -> Self {
        Self {
            spec: constants,
            state: State::new(poseidon::State::default().words()),
            buf: Vec::new(),
            _marker: PhantomData,
        }
    }

    fn squeeze(&mut self) -> C::Scalar {
        self.output()
    }
}

#[derive(Clone, Debug)]
pub struct PoseidonHash<
    C: CurveAffine<ScalarExt = F>,
    F: PrimeField + FromUniformBytes<64>,
    const T: usize,
    const RATE: usize,
> {
    spec: Spec<F, T, RATE>,
    state: State<F, T, RATE>,
    buf: Vec<F>,
    _marker: PhantomData<C>,
}

impl<
        C: CurveAffine<ScalarExt = F>,
        F: PrimeField + FromUniformBytes<64>,
        const T: usize,
        const RATE: usize,
    > PoseidonHash<C, F, T, RATE>
{
    fn update(&mut self, elements: &[F]) {
        self.buf.extend_from_slice(elements);
    }

    fn output(&mut self) -> F {
        let buf = mem::take(&mut self.buf);
        let exact = buf.len() % RATE == 0;

        for chunk in buf.chunks(RATE) {
            self.permutation(chunk);
        }
        if exact {
            self.permutation(&[]);
        }

        self.state.inner[1]
    }

    fn permutation(&mut self, inputs: &[F]) {
        let r_f = self.spec.r_f() / 2;
        let mds = self.spec.mds_matrices().mds().rows();
        let pre_sparse_mds = self.spec.mds_matrices().pre_sparse_mds().rows();
        let sparse_matrices = self.spec.mds_matrices().sparse_matrices();

        // First half of the full rounds
        let constants = self.spec.constants().start();
        self.state.pre_round(inputs, &constants[0]);
        for constants in constants.iter().skip(1).take(r_f - 1) {
            self.state.sbox_full(constants);
            self.state.apply_mds(&mds);
        }
        self.state.sbox_full(constants.last().unwrap());
        self.state.apply_mds(&pre_sparse_mds);

        // Partial rounds
        let constants = self.spec.constants().partial();
        for (constant, sparse_mds) in constants.iter().zip(sparse_matrices.iter()) {
            self.state.sbox_part(constant);
            self.state.apply_sparse_mds(sparse_mds);
        }

        // Second half of the full rounds
        let constants = self.spec.constants().end();
        for constants in constants.iter() {
            self.state.sbox_full(constants);
            self.state.apply_mds(&mds);
        }
        self.state.sbox_full(&[F::ZERO; T]);
        self.state.apply_mds(&mds);
    }
}

#[cfg(test)]
mod tests {
    use halo2curves::{
        bn256::{Fr, G1Affine},
        pasta::{EqAffine, Fp},
    };

    use super::*;

    #[test]
    fn test_poseidon_hash() {
        const T: usize = 4;
        const RATE: usize = 3;
        const R_F: usize = 8;
        const R_P: usize = 56;
        type PH = PoseidonHash<G1Affine, Fr, T, RATE>;
        let spec = Spec::<Fr, T, RATE>::new(R_F, R_P);
        let mut poseidon = PH::new(spec);
        for i in 0..5 {
            poseidon.update(&[Fr::from(i as u64)]);
        }
        let output = poseidon.squeeze();
        // 0x2ce4016298e9e5fcaa94ccb686413e16add1bb813def8a3a0628aed46ea07749
        let out_hash = Fr::from_str_vartime(
            "20304616028358001435806807494046171997958789835068077254356069730773893150537",
        )
        .unwrap();
        assert_eq!(output, out_hash);
    }
}
