use super::{general::SurMap, linear::BimapInt};
use crate::utils::diagonals;

/// A [SurMap] to and from tilings.
///
/// For example:
/// ```
/// # use morello::grid::downscale::DownscaleSurMap;
/// # use crate::morello::grid::general::SurMap;
/// let s = DownscaleSurMap(&[2, 2]);
/// assert_eq!(s.apply(&vec![2, 0]), [1, 0]);
/// assert_eq!(
///   s.apply_inverse(&vec![1, 0]).collect::<Vec<_>>(),
///   vec![vec![2, 0], vec![2, 1], vec![3, 0], vec![3, 1]]);
/// ```
pub struct DownscaleSurMap<'a>(pub &'a [BimapInt]);

impl<'a> SurMap for DownscaleSurMap<'a> {
    // TODO: Be generic over integer type
    type Domain = Vec<BimapInt>;
    type Codomain = Vec<BimapInt>;
    type DomainIter = Box<dyn Iterator<Item = Vec<BimapInt>> + Send + 'a>;

    fn apply(&self, t: &Self::Domain) -> Self::Codomain {
        assert_eq!(t.len(), self.0.len());
        t.iter().zip(self.0).map(|(t, s)| t / s).collect()
    }

    fn apply_inverse(&self, i: &Self::Codomain) -> Self::DomainIter {
        assert_eq!(i.len(), self.0.len());

        let tile_shape_inclusive = self.0.iter().map(|s| *s - 1).collect::<Vec<_>>();
        let tile_offset = i.iter().zip(self.0).map(|(i, s)| i * s).collect::<Vec<_>>();

        Box::new(
            diagonals(&tile_shape_inclusive)
                .flatten()
                .map(move |mut within_tile_pt| {
                    // Shift within-tile point by tile offset
                    for (o, p) in tile_offset.iter().zip(&mut within_tile_pt) {
                        *p += o;
                    }
                    within_tile_pt
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_downscalesurmap_forward() {
        let surmap = DownscaleSurMap(&[2, 2]);
        assert_eq!(surmap.apply(&vec![0, 0]), [0, 0]);
        assert_eq!(surmap.apply(&vec![1, 1]), [0, 0]);
        assert_eq!(surmap.apply(&vec![1, 2]), [0, 1]);
    }

    #[test]
    fn test_downscalesurmap_reverse() {
        let surmap = DownscaleSurMap(&[2, 2]);
        assert_eq!(
            surmap.apply_inverse(&vec![0, 0]).collect::<Vec<_>>(),
            vec![vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]
        );
        assert_eq!(
            surmap.apply_inverse(&vec![0, 1]).collect::<Vec<_>>(),
            vec![vec![0, 2], vec![0, 3], vec![1, 2], vec![1, 3]]
        );
    }
}
