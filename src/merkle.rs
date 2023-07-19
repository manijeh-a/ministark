use crate::utils::SerdeOutput;
use crate::Matrix;
use alloc::vec::Vec;
use ark_ff::Field;
use ark_serialize::CanonicalDeserialize;
use ark_serialize::CanonicalSerialize;
use ark_serialize::Valid;
use digest::Digest;
use digest::Output;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use snafu::Snafu;
use std::fmt::Debug;
use std::marker::PhantomData;

/// Merkle tree error
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("tree must contain `{expected}` leaves, but `{actual}` were provided"))]
    TooFewLeaves { expected: usize, actual: usize },
    #[snafu(display("number of leaves must be a power of two, but `{n}` were provided"))]
    NumberOfLeavesNotPowerOfTwo { n: usize },
    #[snafu(display("leaf index `{i}` cannot exceed the number of leaves (`{n}`)"))]
    LeafIndexOutOfBounds { i: usize, n: usize },
    #[snafu(display("proof is invalid"))]
    InvalidProof,
}

pub trait MerkleTree: Clone {
    type Proof: CanonicalSerialize + CanonicalDeserialize + Clone + Send + Sync;
    type Root: Clone;

    /// Returns the root of the merkle tree
    fn root(&self) -> &Self::Root;

    /// Generates a merkle proof
    ///
    /// # Errors
    ///
    /// Returns an error if the leaf index is out of bounds.
    fn prove(&self, index: usize) -> Result<Self::Proof, Error>;

    /// Verifies a merkle proof
    ///
    /// # Errors
    ///
    /// This function returns an error if the proof fails verification.
    fn verify(root: &Self::Root, proof: &Self::Proof, index: usize) -> Result<(), Error>;
}

pub trait MerkleTreeConfig: Send + Sync + Sized + 'static {
    type Digest: Digest;
    type Leaf: CanonicalDeserialize + CanonicalSerialize + Clone + Send + Sync + Sized + 'static;

    fn hash_leaves(l0: &Self::Leaf, l1: &Self::Leaf) -> Output<Self::Digest>;

    fn build_merkle_nodes(leaves: &[Self::Leaf]) -> Vec<Output<Self::Digest>> {
        build_merkle_nodes_default::<Self>(leaves)
    }
}

pub struct MerkleProof<C: MerkleTreeConfig> {
    path: Vec<SerdeOutput<C::Digest>>,
    sibling: C::Leaf,
    leaf: C::Leaf,
}

impl<C: MerkleTreeConfig> Debug for MerkleProof<C>
where
    C::Leaf: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MerkleProof")
            .field("path", &self.path)
            .field("sibling", &self.sibling)
            .field("leaf", &self.leaf)
            .finish()
    }
}

impl<C: MerkleTreeConfig> PartialEq for MerkleProof<C>
where
    C::Leaf: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        (&self.path, &self.leaf, &self.sibling) == (&other.path, &other.leaf, &other.sibling)
    }
}

impl<C: MerkleTreeConfig> MerkleProof<C> {
    pub fn new(leaf: C::Leaf, sibling: C::Leaf, path: Vec<SerdeOutput<C::Digest>>) -> Self {
        Self {
            path,
            sibling,
            leaf,
        }
    }

    pub fn height(&self) -> usize {
        self.path.len() + 1
    }

    pub fn path(&self) -> &[SerdeOutput<C::Digest>] {
        &self.path
    }

    pub fn sibling(&self) -> &C::Leaf {
        &self.sibling
    }

    pub fn leaf(&self) -> &C::Leaf {
        &self.leaf
    }
}

impl<C: MerkleTreeConfig> Clone for MerkleProof<C> {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            sibling: self.sibling.clone(),
            leaf: self.leaf.clone(),
        }
    }
}

impl<C: MerkleTreeConfig> CanonicalSerialize for MerkleProof<C> {
    fn serialize_with_mode<W: ark_serialize::Write>(
        &self,
        mut writer: W,
        compress: ark_serialize::Compress,
    ) -> Result<(), ark_serialize::SerializationError> {
        self.path.serialize_with_mode(&mut writer, compress)?;
        self.sibling.serialize_with_mode(&mut writer, compress)?;
        self.leaf.serialize_with_mode(writer, compress)
    }

    fn serialized_size(&self, compress: ark_serialize::Compress) -> usize {
        self.path.serialized_size(compress)
            + self.sibling.serialized_size(compress)
            + self.leaf.serialized_size(compress)
    }
}

impl<C: MerkleTreeConfig> Valid for MerkleProof<C> {
    #[inline]
    fn check(&self) -> Result<(), ark_serialize::SerializationError> {
        Ok(())
    }
}

impl<C: MerkleTreeConfig> CanonicalDeserialize for MerkleProof<C> {
    fn deserialize_with_mode<R: ark_serialize::Read>(
        mut reader: R,
        compress: ark_serialize::Compress,
        validate: ark_serialize::Validate,
    ) -> Result<Self, ark_serialize::SerializationError> {
        Ok(Self {
            path: <_>::deserialize_with_mode(&mut reader, compress, validate)?,
            sibling: <_>::deserialize_with_mode(&mut reader, compress, validate)?,
            leaf: <_>::deserialize_with_mode(&mut reader, compress, validate)?,
        })
    }
}

/// Merkle tree implemented as a full power-of-two arity tree.
///
/// ```text
///       #        <- root node
///     /   \
///   #       #    <- nodes
///  / \     / \
/// +   +   +   +  <- leaves
/// ```
pub struct MerkleTreeImpl<C: MerkleTreeConfig> {
    pub nodes: Vec<Output<C::Digest>>,
    pub leaves: Vec<C::Leaf>,
}

impl<C: MerkleTreeConfig> Clone for MerkleTreeImpl<C> {
    fn clone(&self) -> Self {
        Self {
            nodes: self.nodes.clone(),
            leaves: self.leaves.clone(),
        }
    }
}

impl<C: MerkleTreeConfig> MerkleTreeImpl<C> {
    /// # Errors
    ///
    /// This function will return an error if:
    /// * there are less than two leaves
    /// * the number of leaves is not a power of two
    pub fn new(leaves: Vec<C::Leaf>) -> Result<Self, Error> {
        let n = leaves.len();
        if n < 2 {
            return Err(Error::TooFewLeaves {
                expected: 2,
                actual: n,
            });
        } else if !n.is_power_of_two() {
            return Err(Error::NumberOfLeavesNotPowerOfTwo { n });
        }

        let nodes = C::build_merkle_nodes(&leaves);
        Ok(Self { nodes, leaves })
    }
}

impl<C: MerkleTreeConfig> MerkleTree for MerkleTreeImpl<C> {
    type Proof = MerkleProof<C>;
    type Root = Output<C::Digest>;

    fn root(&self) -> &Output<C::Digest> {
        &self.nodes[1]
    }

    fn prove(&self, index: usize) -> Result<MerkleProof<C>, Error> {
        if index >= self.leaves.len() {
            return Err(Error::LeafIndexOutOfBounds {
                n: self.leaves.len(),
                i: index,
            });
        }

        // TODO: batch proofs
        // TODO: omit leaves[index]?
        let leaf = self.leaves[index].clone();
        let sibling = self.leaves[index ^ 1].clone();

        let mut path = Vec::new();
        let mut index = (index + self.nodes.len()) >> 1;
        while index > 1 {
            path.push(SerdeOutput::new(self.nodes[index ^ 1].clone()));
            index >>= 1;
        }

        Ok(MerkleProof {
            path,
            sibling,
            leaf,
        })
    }

    fn verify(
        root: &Output<C::Digest>,
        proof: &MerkleProof<C>,
        mut index: usize,
    ) -> Result<(), Error> {
        // hash the leaves
        let mut running_hash = if index % 2 == 0 {
            C::hash_leaves(&proof.leaf, &proof.sibling)
        } else {
            C::hash_leaves(&proof.sibling, &proof.leaf)
        };

        index >>= 1;
        for node in &proof.path {
            let mut hasher = C::Digest::new();
            if index % 2 == 0 {
                hasher.update(running_hash);
                hasher.update(&**node);
            } else {
                hasher.update(&**node);
                hasher.update(running_hash);
            }
            running_hash = hasher.finalize();
            index >>= 1;
        }

        if *root == running_hash {
            Ok(())
        } else {
            Err(Error::InvalidProof)
        }
    }
}

pub trait MatrixMerkleTree<T>: MerkleTree + Sized {
    fn from_matrix(m: &Matrix<T>) -> Self;

    fn prove_row(&self, row_idx: usize) -> Result<Self::Proof, Error>;

    fn verify_row(
        root: &Self::Root,
        row_idx: usize,
        row: &[T],
        proof: &Self::Proof,
    ) -> Result<(), Error>;
}

pub struct HashedLeafConfig<D: Digest>(PhantomData<D>);

impl<D: Digest> Clone for HashedLeafConfig<D> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}

impl<D: Digest + Send + Sync + 'static> MerkleTreeConfig for HashedLeafConfig<D> {
    type Digest = D;
    type Leaf = SerdeOutput<D>;

    fn hash_leaves(l0: &Self::Leaf, l1: &Self::Leaf) -> Output<Self::Digest> {
        let mut hasher = D::new();
        hasher.update(&**l0);
        hasher.update(&**l1);
        hasher.finalize()
    }
}

pub struct MatrixMerkleTreeImpl<D: Digest + Send + Sync + 'static> {
    merkle_tree: MerkleTreeImpl<HashedLeafConfig<D>>,
}

impl<D: Digest + Send + Sync + 'static> Clone for MatrixMerkleTreeImpl<D> {
    fn clone(&self) -> Self {
        Self {
            merkle_tree: self.merkle_tree.clone(),
        }
    }
}

impl<D: Digest + Send + Sync + 'static> MatrixMerkleTreeImpl<D> {
    fn new(leaves: Vec<Output<D>>) -> Result<Self, Error> {
        let leaves = leaves.into_iter().map(SerdeOutput::new).collect();
        Ok(Self {
            merkle_tree: MerkleTreeImpl::new(leaves)?,
        })
    }
}

impl<D: Digest + Send + Sync + 'static> MerkleTree for MatrixMerkleTreeImpl<D> {
    type Proof = MerkleProof<HashedLeafConfig<D>>;
    type Root = Output<D>;

    fn root(&self) -> &Self::Root {
        self.merkle_tree.root()
    }

    fn prove(&self, index: usize) -> Result<Self::Proof, Error> {
        self.merkle_tree.prove(index)
    }

    fn verify(root: &Self::Root, proof: &Self::Proof, index: usize) -> Result<(), Error> {
        MerkleTreeImpl::verify(root, proof, index)
    }
}

impl<D: Digest + Send + Sync + 'static, F: Field> MatrixMerkleTree<F> for MatrixMerkleTreeImpl<D> {
    fn from_matrix(m: &Matrix<F>) -> Self {
        Self::new(hash_rows::<D, F>(m)).unwrap()
    }

    fn prove_row(&self, row_idx: usize) -> Result<Self::Proof, Error> {
        <Self as MerkleTree>::prove(self, row_idx)
    }

    fn verify_row(
        root: &Self::Root,
        row_idx: usize,
        row: &[F],
        proof: &Self::Proof,
    ) -> Result<(), Error> {
        let row_hash = hash_row::<D, F>(row);
        if *proof.leaf == row_hash {
            <Self as MerkleTree>::verify(root, proof, row_idx)
        } else {
            Err(Error::InvalidProof)
        }
    }
}

#[inline]
fn hash_row_with_buffer<D: Digest, F: Field>(row: &[F], buffer: &mut Vec<u8>) -> Output<D> {
    row.serialize_compressed(&mut *buffer).unwrap();
    D::digest(buffer)
}

fn hash_row<D: Digest, F: Field>(row: &[F]) -> Output<D> {
    let mut buffer = Vec::new();
    hash_row_with_buffer::<D, F>(row, &mut buffer)
}

fn hash_rows<D: Digest, F: Field>(matrix: &Matrix<F>) -> Vec<Output<D>> {
    let num_rows = matrix.num_rows();
    let mut row_hashes = vec![Output::<D>::default(); num_rows];

    #[cfg(not(feature = "parallel"))]
    let chunk_size = row_hashes.len();
    #[cfg(feature = "parallel")]
    let chunk_size = core::cmp::max(
        row_hashes.len() / rayon::current_num_threads().next_power_of_two(),
        128,
    );

    ark_std::cfg_chunks_mut!(row_hashes, chunk_size)
        .enumerate()
        .for_each(|(chunk_offset, chunk)| {
            let offset = chunk_size * chunk_offset;

            let mut row_buffer = vec![F::zero(); matrix.num_cols()];
            let mut row_bytes = Vec::with_capacity(row_buffer.compressed_size());

            for (i, row_hash) in chunk.iter_mut().enumerate() {
                row_bytes.clear();
                matrix.read_row(offset + i, &mut row_buffer);
                *row_hash = hash_row_with_buffer::<D, F>(&row_buffer, &mut row_bytes);
            }
        });

    row_hashes
}

#[cfg(feature = "parallel")]
fn build_merkle_nodes_default<C: MerkleTreeConfig>(leaves: &[C::Leaf]) -> Vec<Output<C::Digest>> {
    let n = leaves.len();
    let num_subtrees = core::cmp::min(rayon::current_num_threads().next_power_of_two(), n / 2);
    let mut nodes = vec![Output::<C::Digest>::default(); n];

    // code adapted from winterfell
    rayon::scope(|s| {
        for i in 0..num_subtrees {
            let nodes = unsafe { &mut *core::ptr::addr_of_mut!(nodes[..]) };
            s.spawn(move |_| {
                // generate first layer of nodes from leaf nodes
                let batch_size = n / num_subtrees;
                let leaf_offset = batch_size * i;
                for j in (0..batch_size).step_by(2) {
                    let lhs = &leaves[leaf_offset + j];
                    let rhs = &leaves[leaf_offset + j + 1];
                    let hash = C::hash_leaves(lhs, rhs);
                    nodes[(n + leaf_offset + j) / 2] = hash;
                }

                // generate remaining nodes
                let mut batch_size = n / num_subtrees / 4;
                let mut start_idx = n / 4 + batch_size * i;
                while start_idx >= num_subtrees {
                    for k in (start_idx..(start_idx + batch_size)).rev() {
                        let mut hasher = C::Digest::new();
                        hasher.update(&nodes[k * 2]);
                        hasher.update(&nodes[k * 2 + 1]);
                        nodes[k] = hasher.finalize();
                    }
                    start_idx /= 2;
                    batch_size /= 2;
                }
            });
        }
    });

    // finish the tip of the tree
    for i in (1..num_subtrees).rev() {
        let mut hasher = C::Digest::new();
        hasher.update(&nodes[i * 2]);
        hasher.update(&nodes[i * 2 + 1]);
        nodes[i] = hasher.finalize();
    }

    nodes
}

#[cfg(not(feature = "parallel"))]
fn build_merkle_nodes_default<C: MerkleTreeConfig>(leaves: &[C::Leaf]) -> Vec<Output<C::Digest>> {
    let n = leaves.len();
    let mut nodes = vec![Output::<C::Digest>::default(); n];

    // generate first layer of nodes from leaf nodes
    for i in 0..n / 2 {
        nodes[n / 2 + i] = C::hash_leaves(&leaves[i * 2], &leaves[i * 2 + 1]);
    }

    // generate remaining nodes
    for i in (1..n / 2).rev() {
        let mut hasher = C::Digest::new();
        hasher.update(&nodes[i * 2]);
        hasher.update(&nodes[i * 2 + 1]);
        nodes[i] = hasher.finalize();
    }

    nodes
}

#[cfg(test)]
mod tests {
    use super::Error;
    use super::MerkleTree;
    use super::MerkleTreeConfig;
    use super::MerkleTreeImpl;
    use crate::utils::SerdeOutput;
    use digest::Digest;
    use digest::Output;
    use sha2::Sha256;

    #[test]
    fn verify() -> Result<(), Error> {
        let leaves = vec![1u32, 2, 3, 4, 5, 6, 7, 8];
        let tree = MerkleTreeImpl::<UnhashedLeafConfig>::new(leaves)?;
        let commitment = tree.root();
        let i = 3;

        let proof = tree.prove(i)?;

        MerkleTreeImpl::verify(commitment, &proof, i)
    }

    #[test]
    fn verify_hashed_leaves() -> Result<(), Error> {
        let leaves = [1u32, 2, 3, 4, 5, 6, 7, 8];
        let hashed_leaves = leaves
            .iter()
            .map(|&v| Sha256::digest(v.to_be_bytes()))
            .map(SerdeOutput::new)
            .collect();
        let tree = MerkleTreeImpl::<HashedLeafConfig>::new(hashed_leaves)?;
        let commitment = tree.root();
        let i = 3;

        let proof = tree.prove(i)?;

        MerkleTreeImpl::verify(commitment, &proof, i)
    }

    #[test]
    fn verify_large_tree() -> Result<(), Error> {
        let leaves = (0..1 << 10).collect::<Vec<u32>>();
        let tree = MerkleTreeImpl::<UnhashedLeafConfig>::new(leaves)?;
        let commitment = tree.root();
        let i = 378;

        let proof = tree.prove(i)?;

        MerkleTreeImpl::verify(commitment, &proof, i)
    }

    struct HashedLeafConfig;

    impl MerkleTreeConfig for HashedLeafConfig {
        type Digest = Sha256;
        type Leaf = SerdeOutput<Sha256>;

        fn hash_leaves(l0: &SerdeOutput<Sha256>, l1: &SerdeOutput<Sha256>) -> Output<Sha256> {
            let mut hasher = Sha256::new();
            hasher.update(&**l0);
            hasher.update(&**l1);
            hasher.finalize()
        }
    }

    struct UnhashedLeafConfig;

    impl MerkleTreeConfig for UnhashedLeafConfig {
        type Digest = Sha256;
        type Leaf = u32;

        fn hash_leaves(l0: &u32, l1: &u32) -> Output<Sha256> {
            let mut hasher = Sha256::new();
            hasher.update(l0.to_be_bytes());
            hasher.update(l1.to_be_bytes());
            hasher.finalize()
        }
    }
}
