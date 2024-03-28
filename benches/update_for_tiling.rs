use iai_callgrind::{black_box, main};
use smallvec::smallvec;

use morello::layout::Layout;

#[inline(never)]
fn update_for_tiling() {
    let shape = [64, 64, 64];
    let tile_shape = [64, 8, 8];
    let layout = Layout::New(smallvec![(0, None), (1, None), (2, None), (1, Some(8))]);
    let c = layout.contiguous_full();
    black_box(layout.update_for_tiling(&shape, &tile_shape, c)).unwrap();
}

main!(
    callgrind_args = "--simulate-wb=no", "--simulate-hwpref=yes",
        "--I1=32768,8,64", "--D1=32768,8,64", "--LL=8388608,16,64";
    functions = update_for_tiling
);
