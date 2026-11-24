//! Synthetic wave64 compute programs (assembled with LLVM's `llvm-mc
//! -mcpu=gfx1010 -mattr=+wavefrontsize64`) for the emulation of 64-lane
//! waves on hosts without them ([`super::host_wave64`]).
//!
//! Each runs as one group of 64 threads (one console wave). The frame puts a
//! raw buffer UAV descriptor in `s[8:11]`; thread n stores its results at
//! dwords n, 64 + n, 128 + n and 192 + n.

use super::lower::{Lowered, Parts, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

/// Lane reads, a ballot and LDS shared within the wave without a barrier.
/// Per lane n: lane 37's v0 (37), the count of lanes below 40 (40), the
/// lane's own index from `v_mbcnt` (n), and lane (n + 1) % 64's v0 through
/// LDS (written by every lane just before).
pub(super) const CROSS_LANE: &[u32] = &[
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x340E_0082, // v_lshlrev_b32_e32 v7, 2, v0
    0xD760_000C,
    0x0001_4B00, // v_readlane_b32 s12, v0, 37
    0x7E02_020C, // v_mov_b32_e32 v1, s12
    0xE070_1000,
    0x8002_0107, // buffer_store_dword v1, v7, s[8:11], 0 offen
    0x7D88_00A8, // v_cmp_gt_u32_e32 vcc, 40, v0
    0xBE8D_106A, // s_bcnt1_i32_b64 s13, vcc
    0x7E04_020D, // v_mov_b32_e32 v2, s13
    0xE070_1100,
    0x8002_0207, // buffer_store_dword v2, v7, s[8:11], 0 offen offset:256
    0xD765_0003,
    0x0001_00C1, // v_mbcnt_lo_u32_b32 v3, -1, 0
    0xD766_0003,
    0x0002_06C1, // v_mbcnt_hi_u32_b32 v3, -1, v3
    0xE070_1200,
    0x8002_0307, // buffer_store_dword v3, v7, s[8:11], 0 offen offset:512
    0x4A0C_0081, // v_add_nc_u32_e32 v6, 1, v0
    0x360C_0CBF, // v_and_b32_e32 v6, 63, v6
    0x340C_0C82, // v_lshlrev_b32_e32 v6, 2, v6
    0xD834_0000,
    0x0000_0007, // ds_write_b32 v7, v0
    0xD8D8_0000,
    0x0500_0006, // ds_read_b32 v5, v6
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xE070_1300,
    0x8002_0507, // buffer_store_dword v5, v7, s[8:11], 0 offen offset:768
];

/// What [`CROSS_LANE`] stores, by dword.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn cross_lane_expected() -> Vec<u32> {
    let mut out = vec![37; 64];
    out.extend([40; 64]);
    out.extend(0..64);
    out.extend((0..64).map(|n| (n + 1) % 64));
    out
}

/// DPP, `v_readfirstlane` and `ds_swizzle`. Per lane n: lane n - 1's v0
/// within its row of 16 (0 for a row's first lane, `bound_ctrl`), lane 0's
/// v0 + 100 (100), and lane n ^ 1's v0 (`ds_swizzle` SWAP 1).
pub(super) const DPP_FIRST_SWIZZLE: &[u32] = &[
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x340E_0082, // v_lshlrev_b32_e32 v7, 2, v0
    0x7E10_02FA,
    0xFF09_1100, // v_mov_b32_dpp v8, v0 row_shr:1 row_mask:0xf bank_mask:0xf bound_ctrl:1
    0xE070_1000,
    0x8002_0807, // buffer_store_dword v8, v7, s[8:11], 0 offen
    0x4A12_00FF,
    0x0000_0064, // v_add_nc_u32_e32 v9, 0x64, v0
    0x7E1C_0509, // v_readfirstlane_b32 s14, v9
    0x7E14_020E, // v_mov_b32_e32 v10, s14
    0xE070_1100,
    0x8002_0A07, // buffer_store_dword v10, v7, s[8:11], 0 offen offset:256
    0xD8D4_041F,
    0x0B00_0000, // ds_swizzle_b32 v11, v0 offset:swizzle(SWAP,1)
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0xE070_1200,
    0x8002_0B07, // buffer_store_dword v11, v7, s[8:11], 0 offen offset:512
];

/// What [`DPP_FIRST_SWIZZLE`] stores, by dword (192 dwords).
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn dpp_first_swizzle_expected() -> Vec<u32> {
    let mut out: Vec<u32> = (0..64)
        .map(|n| if n % 16 == 0 { 0 } else { n - 1 })
        .collect();
    out.extend([100; 64]);
    out.extend((0..64).map(|n| n ^ 1));
    out
}

/// A lane read inside `if (lane < 10)`: only those lanes reach it, so a
/// group barrier there could never complete.
pub(super) const DIVERGENT_READLANE: &[u32] = &[
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    0x340E_0082, // v_lshlrev_b32_e32 v7, 2, v0
    0x7E02_0280, // v_mov_b32_e32 v1, 0
    0x7D88_008A, // v_cmp_gt_u32_e32 vcc, 10, v0
    0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
    0xBF88_0003, // s_cbranch_execz 3
    0xD760_000C,
    0x0001_0B00, // v_readlane_b32 s12, v0, 5
    0x7E02_020C, // v_mov_b32_e32 v1, s12
    0x88FE_147E, // s_or_b64 exec, exec, s[20:21]
    0xE070_1000,
    0x8002_0107, // buffer_store_dword v1, v7, s[8:11], 0 offen
];

/// `body` as a wave64 compute program of `threads` threads with
/// `lds_bytes` of LDS, root constants in s0-s3 and a UAV table in s4.
pub(super) fn program(body: &[u32], threads: u32, lds_bytes: u32) -> NativeProgram {
    // s_mov_b32 s5, 0; s_load_dwordx4 s[8:11], s[4:5], null
    let mut words = vec![0xBE85_0380, 0xF408_0202, 0xFA00_0000];
    words.extend_from_slice(body);
    words.push(0xBF81_0000); // s_endpgm
    let param = |root, sgprs, user_data, slot| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 32,
        layout: vec![
            param(RootType::Constants, 4, 0, 0),
            param(RootType::Table, 1, 4, 1),
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            // 5 user SGPRs; LDS in 512-byte units.
            rsrc2: 5 << 1 | lds_bytes.div_ceil(512) << 15,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// Lowers `native` for a host with (or without) 64-lane waves.
pub(super) fn lower_for(native: &NativeProgram, host_wave64: bool) -> Result<Lowered, ShaderError> {
    let p = ir::build(&native.code, native.wave64, native.vgprs)?;
    let res = resource::track(&p, &native.layout, native.stage)?;
    let io = io::raise(&p, native)?;
    let st = structure::structure(&p)?;
    lower(&Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        host_wave64,
    })
}

#[test]
fn one_wave_groups_translate_without_64_lane_waves() {
    for body in [CROSS_LANE, DPP_FIRST_SWIZZLE] {
        let native = program(body, 64, 512);
        lower_for(&native, true).unwrap();
        lower_for(&native, false).unwrap();
    }
}

#[test]
fn what_cannot_be_emulated_is_refused_with_a_reason() {
    // Several console waves in one group: their lanes cannot be exchanged
    // through one group's barriers.
    let several = program(CROSS_LANE, 128, 512);
    lower_for(&several, true).unwrap();
    let Err(e) = lower_for(&several, false) else {
        panic!("translated");
    };
    assert!(e.to_string().contains("several 64-lane waves"), "{e}");
    // A lane read only some lanes reach.
    let divergent = program(DIVERGENT_READLANE, 64, 0);
    lower_for(&divergent, true).unwrap();
    let Err(e) = lower_for(&divergent, false) else {
        panic!("translated");
    };
    assert!(e.to_string().contains("lane-dependent code"), "{e}");
}
