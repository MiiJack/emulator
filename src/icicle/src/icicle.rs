use icicle_cpu::ValueSource;
use std::collections::HashMap;

fn create_x64_vm() -> icicle_vm::Vm {
    let mut cpu_config = icicle_vm::cpu::Config::from_target_triple("x86_64-none");
    cpu_config.enable_jit = true;
    cpu_config.enable_jit_mem = true;
    cpu_config.enable_shadow_stack = false;
    cpu_config.enable_recompilation = true;
    cpu_config.track_uninitialized = false;
    cpu_config.optimize_instructions = true;
    cpu_config.optimize_block = false;

    return icicle_vm::build(&cpu_config).unwrap();
}

fn map_permissions(foreign_permissions: u8) -> u8 {
    const FOREIGN_READ: u8 = 1 << 0;
    const FOREIGN_WRITE: u8 = 1 << 1;
    const FOREIGN_EXEC: u8 = 1 << 2;

    let mut permissions: u8 = 0;

    if (foreign_permissions & FOREIGN_READ) != 0 {
        permissions |= icicle_vm::cpu::mem::perm::READ;
    }

    if (foreign_permissions & FOREIGN_WRITE) != 0 {
        permissions |= icicle_vm::cpu::mem::perm::WRITE;
    }

    if (foreign_permissions & FOREIGN_EXEC) != 0 {
        permissions |= icicle_vm::cpu::mem::perm::EXEC;
    }

    return permissions;
}

#[repr(u8)]
#[allow(dead_code)]
#[derive(PartialEq)]
enum HookType {
    Syscall = 1,
    Read,
    Write,
    Execute,
    Unknown,
}

fn u8_to_hook_type_unsafe(value: u8) -> HookType {
    // This is unsafe because it assumes the value is valid
    unsafe { std::mem::transmute(value) }
}

fn split_hook_id(id: u32) -> (u32, HookType) {
    let hook_id = id & 0xFFFFFF;
    let hook_type = u8_to_hook_type_unsafe((id >> 24) as u8);

    return (hook_id, hook_type);
}

fn qualify_hook_id(hook_id: u32, hook_type: HookType) -> u32 {
    let hook_type: u32 = (hook_type as u8).into();
    let hook_type_mask: u32 = hook_type << 24;
    return (hook_id | hook_type_mask).into();
}

pub struct HookContainer<Func: ?Sized> {
    hook_id: u32,
    hooks: HashMap<u32, Box<Func>>,
}

impl<Func: ?Sized> HookContainer<Func> {
    pub fn new() -> Self {
        Self {
            hook_id: 0,
            hooks: HashMap::new(),
        }
    }

    pub fn add_hook(&mut self, callback: Box<Func>) -> u32 {
        self.hook_id += 1;
        let id = self.hook_id;
        self.hooks.insert(id, callback);

        return id;
    }

    pub fn get_hooks(&self) -> &HashMap<u32, Box<Func>> {
        return &self.hooks;
    }

    pub fn remove_hook(&mut self, id: u32) {
        self.hooks.remove(&id);
    }
}

pub struct IcicleEmulator {
    vm: icicle_vm::Vm,
    reg: X64RegisterNodes,
    syscall_hooks: HookContainer<dyn Fn()>,
}

pub struct MmioHandler {
    read_handler: Box<dyn Fn(u64, &mut [u8])>,
    write_handler: Box<dyn Fn(u64, &[u8])>,
}

impl MmioHandler {
    pub fn new(
        read_function: Box<dyn Fn(u64, &mut [u8])>,
        write_function: Box<dyn Fn(u64, &[u8])>,
    ) -> Self {
        Self {
            read_handler: read_function,
            write_handler: write_function,
        }
    }
}

impl icicle_cpu::mem::IoMemory for MmioHandler {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> icicle_cpu::mem::MemResult<()> {
        (self.read_handler)(addr, buf);
        return Ok(());
    }

    fn write(&mut self, addr: u64, value: &[u8]) -> icicle_cpu::mem::MemResult<()> {
        (self.write_handler)(addr, value);
        return Ok(());
    }
}

impl IcicleEmulator {
    pub fn new() -> Self {
        let virtual_machine = create_x64_vm();
        Self {
            reg: X64RegisterNodes::new(&virtual_machine.cpu.arch),
            vm: virtual_machine,
            syscall_hooks: HookContainer::new(),
        }
    }

    fn get_mem(&mut self) -> &mut icicle_vm::cpu::Mmu {
        return &mut self.vm.cpu.mem;
    }

    pub fn start(&mut self) {
        loop {
            let reason = self.vm.run();

            let invoke_syscall = match reason {
                icicle_vm::VmExit::UnhandledException((code, _)) => {
                    code == icicle_cpu::ExceptionCode::Syscall
                }
                _ => false,
            };

            if !invoke_syscall {
                break;
            }

            for (_key, func) in self.syscall_hooks.get_hooks() {
                func();
            }

            self.vm.cpu.write_pc(self.vm.cpu.read_pc() + 2);
        }
    }

    pub fn add_syscall_hook(&mut self, callback: Box<dyn Fn()>) -> u32 {
        let hook_id = self.syscall_hooks.add_hook(callback);
        return qualify_hook_id(hook_id, HookType::Syscall);
    }

    pub fn remove_hook(&mut self, id: u32) {
        let (hook_id, hook_type) = split_hook_id(id);

        match hook_type {
            HookType::Syscall => self.syscall_hooks.remove_hook(hook_id),
            _ => {}
        }
    }

    pub fn map_memory(&mut self, address: u64, length: u64, permissions: u8) -> bool {
        const MAPPING_PERMISSIONS: u8 = icicle_vm::cpu::mem::perm::MAP
            | icicle_vm::cpu::mem::perm::INIT
            | icicle_vm::cpu::mem::perm::IN_CODE_CACHE;

        let native_permissions = map_permissions(permissions);

        let mapping = icicle_vm::cpu::mem::Mapping {
            perm: native_permissions | MAPPING_PERMISSIONS,
            value: 0x0,
        };

        let layout = icicle_vm::cpu::mem::AllocLayout {
            addr: Some(address),
            size: length,
            align: 0x1000,
        };

        let res = self.get_mem().alloc_memory(layout, mapping);
        return res.is_ok();
    }

    pub fn map_mmio(
        &mut self,
        address: u64,
        length: u64,
        read_function: Box<dyn Fn(u64, &mut [u8])>,
        write_function: Box<dyn Fn(u64, &[u8])>,
    ) -> bool {
        let mem = self.get_mem();

        let handler = MmioHandler::new(read_function, write_function);
        let handler_id = mem.register_io_handler(handler);

        let layout = icicle_vm::cpu::mem::AllocLayout {
            addr: Some(address),
            size: length,
            align: 0x1000,
        };

        let res = mem.alloc_memory(layout, handler_id);
        return res.is_ok();
    }

    pub fn unmap_memory(&mut self, address: u64, length: u64) -> bool {
        return self.get_mem().unmap_memory_len(address, length);
    }

    pub fn protect_memory(&mut self, address: u64, length: u64, permissions: u8) -> bool {
        let native_permissions = map_permissions(permissions);
        let res = self
            .get_mem()
            .update_perm(address, length, native_permissions);
        return res.is_ok();
    }

    pub fn write_memory(&mut self, address: u64, data: &[u8]) -> bool {
        let res = self
            .get_mem()
            .write_bytes(address, data, icicle_vm::cpu::mem::perm::NONE);
        return res.is_ok();
    }

    pub fn read_memory(&mut self, address: u64, data: &mut [u8]) -> bool {
        let res = self
            .get_mem()
            .read_bytes(address, data, icicle_vm::cpu::mem::perm::NONE);
        return res.is_ok();
    }

    pub fn read_register(&mut self, reg: X64Register, buffer: &mut [u8]) -> usize {
        let reg_node = self.reg.get_node(reg);

        let res = self.vm.cpu.read_dynamic(pcode::Value::Var(reg_node));
        let bytes: [u8; 32] = res.zxt();

        let len = std::cmp::min(bytes.len(), buffer.len());
        buffer[..len].copy_from_slice(&bytes[..len]);

        return reg_node.size.into();
    }

    pub fn write_register(&mut self, reg: X64Register, data: &[u8]) -> usize {
        let reg_node = self.reg.get_node(reg);

        let mut buffer = [0u8; 32];
        let len = std::cmp::min(data.len(), buffer.len());
        buffer[..len].copy_from_slice(&data[..len]);

        //let value = icicle_cpu::regs::DynamicValue::new(buffer, reg_node.size.into());
        //self.vm.cpu.write_trunc(reg_node, value);

        match reg_node.size {
            1 => self
                .vm
                .cpu
                .write_var::<[u8; 1]>(reg_node, buffer[..1].try_into().expect("")),
            2 => self
                .vm
                .cpu
                .write_var::<[u8; 2]>(reg_node, buffer[..2].try_into().expect("")),
            3 => self
                .vm
                .cpu
                .write_var::<[u8; 3]>(reg_node, buffer[..3].try_into().expect("")),
            4 => self
                .vm
                .cpu
                .write_var::<[u8; 4]>(reg_node, buffer[..4].try_into().expect("")),
            5 => self
                .vm
                .cpu
                .write_var::<[u8; 5]>(reg_node, buffer[..5].try_into().expect("")),
            6 => self
                .vm
                .cpu
                .write_var::<[u8; 6]>(reg_node, buffer[..6].try_into().expect("")),
            7 => self
                .vm
                .cpu
                .write_var::<[u8; 7]>(reg_node, buffer[..7].try_into().expect("")),
            8 => self
                .vm
                .cpu
                .write_var::<[u8; 8]>(reg_node, buffer[..8].try_into().expect("")),
            9 => self
                .vm
                .cpu
                .write_var::<[u8; 9]>(reg_node, buffer[..9].try_into().expect("")),
            10 => self
                .vm
                .cpu
                .write_var::<[u8; 10]>(reg_node, buffer[..10].try_into().expect("")),
            11 => self
                .vm
                .cpu
                .write_var::<[u8; 11]>(reg_node, buffer[..11].try_into().expect("")),
            12 => self
                .vm
                .cpu
                .write_var::<[u8; 12]>(reg_node, buffer[..12].try_into().expect("")),
            13 => self
                .vm
                .cpu
                .write_var::<[u8; 13]>(reg_node, buffer[..13].try_into().expect("")),
            14 => self
                .vm
                .cpu
                .write_var::<[u8; 14]>(reg_node, buffer[..14].try_into().expect("")),
            15 => self
                .vm
                .cpu
                .write_var::<[u8; 15]>(reg_node, buffer[..15].try_into().expect("")),
            16 => self
                .vm
                .cpu
                .write_var::<[u8; 16]>(reg_node, buffer[..16].try_into().expect("")),
            _ => panic!("invalid dynamic value size"),
        }

        return reg_node.size.into();
    }
}

// ------------------------------

#[repr(i32)]
#[derive(PartialEq)]
pub enum X64Register {
    Invalid = 0,
    Ah,
    Al,
    Ax,
    Bh,
    Bl,
    Bp,
    Bpl,
    Bx,
    Ch,
    Cl,
    Cs,
    Cx,
    Dh,
    Di,
    Dil,
    Dl,
    Ds,
    Dx,
    Eax,
    Ebp,
    Ebx,
    Ecx,
    Edi,
    Edx,
    Eflags,
    Eip,
    Es = 26 + 2,
    Esi,
    Esp,
    Fpsw,
    Fs,
    Gs,
    Ip,
    Rax,
    Rbp,
    Rbx,
    Rcx,
    Rdi,
    Rdx,
    Rip,
    Rsi = 41 + 2,
    Rsp,
    Si,
    Sil,
    Sp,
    Spl,
    Ss,
    Cr0,
    Cr1,
    Cr2,
    Cr3,
    Cr4,
    Cr8 = 54 + 4,
    Dr0 = 58 + 8,
    Dr1,
    Dr2,
    Dr3,
    Dr4,
    Dr5,
    Dr6,
    Dr7,
    Fp0 = 73 + 9,
    Fp1,
    Fp2,
    Fp3,
    Fp4,
    Fp5,
    Fp6,
    Fp7,
    K0,
    K1,
    K2,
    K3,
    K4,
    K5,
    K6,
    K7,
    Mm0,
    Mm1,
    Mm2,
    Mm3,
    Mm4,
    Mm5,
    Mm6,
    Mm7,
    R8,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
    St0,
    St1,
    St2,
    St3,
    St4,
    St5,
    St6,
    St7,
    Xmm0,
    Xmm1,
    Xmm2,
    Xmm3,
    Xmm4,
    Xmm5,
    Xmm6,
    Xmm7,
    Xmm8,
    Xmm9,
    Xmm10,
    Xmm11,
    Xmm12,
    Xmm13,
    Xmm14,
    Xmm15,
    Xmm16,
    Xmm17,
    Xmm18,
    Xmm19,
    Xmm20,
    Xmm21,
    Xmm22,
    Xmm23,
    Xmm24,
    Xmm25,
    Xmm26,
    Xmm27,
    Xmm28,
    Xmm29,
    Xmm30,
    Xmm31,
    Ymm0,
    Ymm1,
    Ymm2,
    Ymm3,
    Ymm4,
    Ymm5,
    Ymm6,
    Ymm7,
    Ymm8,
    Ymm9,
    Ymm10,
    Ymm11,
    Ymm12,
    Ymm13,
    Ymm14,
    Ymm15,
    Ymm16,
    Ymm17,
    Ymm18,
    Ymm19,
    Ymm20,
    Ymm21,
    Ymm22,
    Ymm23,
    Ymm24,
    Ymm25,
    Ymm26,
    Ymm27,
    Ymm28,
    Ymm29,
    Ymm30,
    Ymm31,
    Zmm0,
    Zmm1,
    Zmm2,
    Zmm3,
    Zmm4,
    Zmm5,
    Zmm6,
    Zmm7,
    Zmm8,
    Zmm9,
    Zmm10,
    Zmm11,
    Zmm12,
    Zmm13,
    Zmm14,
    Zmm15,
    Zmm16,
    Zmm17,
    Zmm18,
    Zmm19,
    Zmm20,
    Zmm21,
    Zmm22,
    Zmm23,
    Zmm24,
    Zmm25,
    Zmm26,
    Zmm27,
    Zmm28,
    Zmm29,
    Zmm30,
    Zmm31,
    R8b,
    R9b,
    R10b,
    R11b,
    R12b,
    R13b,
    R14b,
    R15b,
    R8d,
    R9d,
    R10d,
    R11d,
    R12d,
    R13d,
    R14d,
    R15d,
    R8w,
    R9w,
    R10w,
    R11w,
    R12w,
    R13w,
    R14w,
    R15w,
    Idtr,
    Gdtr,
    Ldtr,
    Tr,
    Fpcw,
    Fptag,
    Msr,
    Mxcsr,
    FsBase,
    GsBase,
    Flags,
    Rflags,
    Fip,
    Fcs,
    Fdp,
    Fds,
    Fop,
    End, // Must be last
}

#[derive(Clone)]
struct X64RegisterNodes {
    rax: pcode::VarNode,
    rbx: pcode::VarNode,
    rcx: pcode::VarNode,
    rdx: pcode::VarNode,
    rsi: pcode::VarNode,
    rdi: pcode::VarNode,
    rbp: pcode::VarNode,
    rsp: pcode::VarNode,
    r8: pcode::VarNode,
    r9: pcode::VarNode,
    r10: pcode::VarNode,
    r11: pcode::VarNode,
    r12: pcode::VarNode,
    r13: pcode::VarNode,
    r14: pcode::VarNode,
    r15: pcode::VarNode,
    rip: pcode::VarNode,
    eflags: pcode::VarNode,
    cs: pcode::VarNode,
    ds: pcode::VarNode,
    es: pcode::VarNode,
    fs: pcode::VarNode,
    gs: pcode::VarNode,
    ss: pcode::VarNode,
    ah: pcode::VarNode,
    al: pcode::VarNode,
    ax: pcode::VarNode,
    bh: pcode::VarNode,
    bl: pcode::VarNode,
    bpl: pcode::VarNode,
    ch: pcode::VarNode,
    cl: pcode::VarNode,
    cx: pcode::VarNode,
    dh: pcode::VarNode,
    dil: pcode::VarNode,
    dl: pcode::VarNode,
    dx: pcode::VarNode,
    eax: pcode::VarNode,
    ebp: pcode::VarNode,
    ebx: pcode::VarNode,
    ecx: pcode::VarNode,
    edi: pcode::VarNode,
    edx: pcode::VarNode,
    esi: pcode::VarNode,
    esp: pcode::VarNode,
    fpsw: pcode::VarNode,
    gdtr: pcode::VarNode,
    idtr: pcode::VarNode,
    ldtr: pcode::VarNode,
    tr: pcode::VarNode,
    cr0: pcode::VarNode,
    cr1: pcode::VarNode,
    cr2: pcode::VarNode,
    cr3: pcode::VarNode,
    cr4: pcode::VarNode,
    cr8: pcode::VarNode,
    dr0: pcode::VarNode,
    dr1: pcode::VarNode,
    dr2: pcode::VarNode,
    dr3: pcode::VarNode,
    dr4: pcode::VarNode,
    dr5: pcode::VarNode,
    dr6: pcode::VarNode,
    dr7: pcode::VarNode,
    fp0: pcode::VarNode,
    fp1: pcode::VarNode,
    fp2: pcode::VarNode,
    fp3: pcode::VarNode,
    fp4: pcode::VarNode,
    fp5: pcode::VarNode,
    fp6: pcode::VarNode,
    fp7: pcode::VarNode,
    /*k0: pcode::VarNode,
    k1: pcode::VarNode,
    k2: pcode::VarNode,
    k3: pcode::VarNode,
    k4: pcode::VarNode,
    k5: pcode::VarNode,
    k6: pcode::VarNode,
    k7: pcode::VarNode,*/
    mm0: pcode::VarNode,
    mm1: pcode::VarNode,
    mm2: pcode::VarNode,
    mm3: pcode::VarNode,
    mm4: pcode::VarNode,
    mm5: pcode::VarNode,
    mm6: pcode::VarNode,
    mm7: pcode::VarNode,
    st0: pcode::VarNode,
    st1: pcode::VarNode,
    st2: pcode::VarNode,
    st3: pcode::VarNode,
    st4: pcode::VarNode,
    st5: pcode::VarNode,
    st6: pcode::VarNode,
    st7: pcode::VarNode,
    xmm0: pcode::VarNode,
    xmm1: pcode::VarNode,
    xmm2: pcode::VarNode,
    xmm3: pcode::VarNode,
    xmm4: pcode::VarNode,
    xmm5: pcode::VarNode,
    xmm6: pcode::VarNode,
    xmm7: pcode::VarNode,
    xmm8: pcode::VarNode,
    xmm9: pcode::VarNode,
    xmm10: pcode::VarNode,
    xmm11: pcode::VarNode,
    xmm12: pcode::VarNode,
    xmm13: pcode::VarNode,
    xmm14: pcode::VarNode,
    xmm15: pcode::VarNode,
    /*xmm16: pcode::VarNode,
    xmm17: pcode::VarNode,
    xmm18: pcode::VarNode,
    xmm19: pcode::VarNode,
    xmm20: pcode::VarNode,
    xmm21: pcode::VarNode,
    xmm22: pcode::VarNode,
    xmm23: pcode::VarNode,
    xmm24: pcode::VarNode,
    xmm25: pcode::VarNode,
    xmm26: pcode::VarNode,
    xmm27: pcode::VarNode,
    xmm28: pcode::VarNode,
    xmm29: pcode::VarNode,
    xmm30: pcode::VarNode,
    xmm31: pcode::VarNode,*/
    ymm0: pcode::VarNode,
    ymm1: pcode::VarNode,
    ymm2: pcode::VarNode,
    ymm3: pcode::VarNode,
    ymm4: pcode::VarNode,
    ymm5: pcode::VarNode,
    ymm6: pcode::VarNode,
    ymm7: pcode::VarNode,
    ymm8: pcode::VarNode,
    ymm9: pcode::VarNode,
    ymm10: pcode::VarNode,
    ymm11: pcode::VarNode,
    ymm12: pcode::VarNode,
    ymm13: pcode::VarNode,
    ymm14: pcode::VarNode,
    ymm15: pcode::VarNode,
    /*ymm16: pcode::VarNode,
    ymm17: pcode::VarNode,
    ymm18: pcode::VarNode,
    ymm19: pcode::VarNode,
    ymm20: pcode::VarNode,
    ymm21: pcode::VarNode,
    ymm22: pcode::VarNode,
    ymm23: pcode::VarNode,
    ymm24: pcode::VarNode,
    ymm25: pcode::VarNode,
    ymm26: pcode::VarNode,
    ymm27: pcode::VarNode,
    ymm28: pcode::VarNode,
    ymm29: pcode::VarNode,
    ymm30: pcode::VarNode,
    ymm31: pcode::VarNode,*/
    /*zmm0: pcode::VarNode,
    zmm1: pcode::VarNode,
    zmm2: pcode::VarNode,
    zmm3: pcode::VarNode,
    zmm4: pcode::VarNode,
    zmm5: pcode::VarNode,
    zmm6: pcode::VarNode,
    zmm7: pcode::VarNode,
    zmm8: pcode::VarNode,
    zmm9: pcode::VarNode,
    zmm10: pcode::VarNode,
    zmm11: pcode::VarNode,
    zmm12: pcode::VarNode,
    zmm13: pcode::VarNode,
    zmm14: pcode::VarNode,
    zmm15: pcode::VarNode,
    zmm16: pcode::VarNode,
    zmm17: pcode::VarNode,
    zmm18: pcode::VarNode,
    zmm19: pcode::VarNode,
    zmm20: pcode::VarNode,
    zmm21: pcode::VarNode,
    zmm22: pcode::VarNode,
    zmm23: pcode::VarNode,
    zmm24: pcode::VarNode,
    zmm25: pcode::VarNode,
    zmm26: pcode::VarNode,
    zmm27: pcode::VarNode,
    zmm28: pcode::VarNode,
    zmm29: pcode::VarNode,
    zmm30: pcode::VarNode,
    zmm31: pcode::VarNode,*/
    r8b: pcode::VarNode,
    r9b: pcode::VarNode,
    r10b: pcode::VarNode,
    r11b: pcode::VarNode,
    r12b: pcode::VarNode,
    r13b: pcode::VarNode,
    r14b: pcode::VarNode,
    r15b: pcode::VarNode,
    r8d: pcode::VarNode,
    r9d: pcode::VarNode,
    r10d: pcode::VarNode,
    r11d: pcode::VarNode,
    r12d: pcode::VarNode,
    r13d: pcode::VarNode,
    r14d: pcode::VarNode,
    r15d: pcode::VarNode,
    r8w: pcode::VarNode,
    r9w: pcode::VarNode,
    r10w: pcode::VarNode,
    r11w: pcode::VarNode,
    r12w: pcode::VarNode,
    r13w: pcode::VarNode,
    r14w: pcode::VarNode,
    r15w: pcode::VarNode,
    fpcw: pcode::VarNode,
    fptag: pcode::VarNode,
    //msr: pcode::VarNode,
    mxcsr: pcode::VarNode,
    fs_base: pcode::VarNode,
    gs_base: pcode::VarNode,
    flags: pcode::VarNode,
    rflags: pcode::VarNode,
    fip: pcode::VarNode,
    //fcs: pcode::VarNode,
    fdp: pcode::VarNode,
    //fds: pcode::VarNode,
    fop: pcode::VarNode,
}

impl X64RegisterNodes {
    pub fn new(arch: &icicle_cpu::Arch) -> Self {
        let r = |name: &str| arch.sleigh.get_reg(name).unwrap().var;
        Self {
            rax: r("RAX"),
            rbx: r("RBX"),
            rcx: r("RCX"),
            rdx: r("RDX"),
            rsi: r("RSI"),
            rdi: r("RDI"),
            rbp: r("RBP"),
            rsp: r("RSP"),
            r8: r("R8"),
            r9: r("R9"),
            r10: r("R10"),
            r11: r("R11"),
            r12: r("R12"),
            r13: r("R13"),
            r14: r("R14"),
            r15: r("R15"),
            rip: r("RIP"),
            eflags: r("eflags"),
            cs: r("CS"),
            ds: r("DS"),
            es: r("ES"),
            fs: r("FS"),
            gs: r("GS"),
            ss: r("SS"),
            ah: r("AH"),
            al: r("AL"),
            ax: r("AX"),
            bh: r("BH"),
            bl: r("BL"),
            bpl: r("BPL"),
            ch: r("CH"),
            cl: r("CL"),
            cx: r("CX"),
            dh: r("DH"),
            dil: r("DIL"),
            dl: r("DL"),
            dx: r("DX"),
            eax: r("EAX"),
            ebp: r("EBP"),
            ebx: r("EBX"),
            ecx: r("ECX"),
            edi: r("EDI"),
            edx: r("EDX"),
            esi: r("ESI"),
            esp: r("ESP"),
            fpsw: r("FPUStatusWord"),
            gdtr: r("GDTR"),
            idtr: r("IDTR"),
            ldtr: r("LDTR"),
            tr: r("TR"),
            cr0: r("CR0"),
            cr1: r("CR1"),
            cr2: r("CR2"),
            cr3: r("CR3"),
            cr4: r("CR4"),
            cr8: r("CR8"),
            dr0: r("DR0"),
            dr1: r("DR1"),
            dr2: r("DR2"),
            dr3: r("DR3"),
            dr4: r("DR4"),
            dr5: r("DR5"),
            dr6: r("DR6"),
            dr7: r("DR7"),
            fp0: r("ST0"), // ??
            fp1: r("ST1"),
            fp2: r("ST2"),
            fp3: r("ST3"),
            fp4: r("ST4"),
            fp5: r("ST5"),
            fp6: r("ST6"),
            fp7: r("ST7"),
            /*k0: r("K0"),
            k1: r("K1"),
            k2: r("K2"),
            k3: r("K3"),
            k4: r("K4"),
            k5: r("K5"),
            k6: r("K6"),
            k7: r("K7"),*/
            mm0: r("MM0"),
            mm1: r("MM1"),
            mm2: r("MM2"),
            mm3: r("MM3"),
            mm4: r("MM4"),
            mm5: r("MM5"),
            mm6: r("MM6"),
            mm7: r("MM7"),
            st0: r("ST0"),
            st1: r("ST1"),
            st2: r("ST2"),
            st3: r("ST3"),
            st4: r("ST4"),
            st5: r("ST5"),
            st6: r("ST6"),
            st7: r("ST7"),
            xmm0: r("XMM0"),
            xmm1: r("XMM1"),
            xmm2: r("XMM2"),
            xmm3: r("XMM3"),
            xmm4: r("XMM4"),
            xmm5: r("XMM5"),
            xmm6: r("XMM6"),
            xmm7: r("XMM7"),
            xmm8: r("XMM8"),
            xmm9: r("XMM9"),
            xmm10: r("XMM10"),
            xmm11: r("XMM11"),
            xmm12: r("XMM12"),
            xmm13: r("XMM13"),
            xmm14: r("XMM14"),
            xmm15: r("XMM15"),
            /*xmm16: r("XMM16"),
            xmm17: r("XMM17"),
            xmm18: r("XMM18"),
            xmm19: r("XMM19"),
            xmm20: r("XMM20"),
            xmm21: r("XMM21"),
            xmm22: r("XMM22"),
            xmm23: r("XMM23"),
            xmm24: r("XMM24"),
            xmm25: r("XMM25"),
            xmm26: r("XMM26"),
            xmm27: r("XMM27"),
            xmm28: r("XMM28"),
            xmm29: r("XMM29"),
            xmm30: r("XMM30"),
            xmm31: r("XMM31"),*/
            ymm0: r("YMM0"),
            ymm1: r("YMM1"),
            ymm2: r("YMM2"),
            ymm3: r("YMM3"),
            ymm4: r("YMM4"),
            ymm5: r("YMM5"),
            ymm6: r("YMM6"),
            ymm7: r("YMM7"),
            ymm8: r("YMM8"),
            ymm9: r("YMM9"),
            ymm10: r("YMM10"),
            ymm11: r("YMM11"),
            ymm12: r("YMM12"),
            ymm13: r("YMM13"),
            ymm14: r("YMM14"),
            ymm15: r("YMM15"),
            /*ymm16: r("YMM16"),
            ymm17: r("YMM17"),
            ymm18: r("YMM18"),
            ymm19: r("YMM19"),
            ymm20: r("YMM20"),
            ymm21: r("YMM21"),
            ymm22: r("YMM22"),
            ymm23: r("YMM23"),
            ymm24: r("YMM24"),
            ymm25: r("YMM25"),
            ymm26: r("YMM26"),
            ymm27: r("YMM27"),
            ymm28: r("YMM28"),
            ymm29: r("YMM29"),
            ymm30: r("YMM30"),
            ymm31: r("YMM31"),*/
            /*zmm0: r("ZMM0"),
            zmm1: r("ZMM1"),
            zmm2: r("ZMM2"),
            zmm3: r("ZMM3"),
            zmm4: r("ZMM4"),
            zmm5: r("ZMM5"),
            zmm6: r("ZMM6"),
            zmm7: r("ZMM7"),
            zmm8: r("ZMM8"),
            zmm9: r("ZMM9"),
            zmm10: r("ZMM10"),
            zmm11: r("ZMM11"),
            zmm12: r("ZMM12"),
            zmm13: r("ZMM13"),
            zmm14: r("ZMM14"),
            zmm15: r("ZMM15"),
            zmm16: r("ZMM16"),
            zmm17: r("ZMM17"),
            zmm18: r("ZMM18"),
            zmm19: r("ZMM19"),
            zmm20: r("ZMM20"),
            zmm21: r("ZMM21"),
            zmm22: r("ZMM22"),
            zmm23: r("ZMM23"),
            zmm24: r("ZMM24"),
            zmm25: r("ZMM25"),
            zmm26: r("ZMM26"),
            zmm27: r("ZMM27"),
            zmm28: r("ZMM28"),
            zmm29: r("ZMM29"),
            zmm30: r("ZMM30"),
            zmm31: r("ZMM31"),*/
            r8b: r("R8B"),
            r9b: r("R9B"),
            r10b: r("R10B"),
            r11b: r("R11B"),
            r12b: r("R12B"),
            r13b: r("R13B"),
            r14b: r("R14B"),
            r15b: r("R15B"),
            r8d: r("R8D"),
            r9d: r("R9D"),
            r10d: r("R10D"),
            r11d: r("R11D"),
            r12d: r("R12D"),
            r13d: r("R13D"),
            r14d: r("R14D"),
            r15d: r("R15D"),
            r8w: r("R8W"),
            r9w: r("R9W"),
            r10w: r("R10W"),
            r11w: r("R11W"),
            r12w: r("R12W"),
            r13w: r("R13W"),
            r14w: r("R14W"),
            r15w: r("R15W"),
            fpcw: r("FPUControlWord"),
            fptag: r("FPUTagWord"),
            mxcsr: r("MXCSR"),
            flags: r("flags"),
            rflags: r("rflags"),
            fip: r("FPUInstructionPointer"),
            fdp: r("FPUDataPointer"),
            fop: r("FPULastInstructionOpcode"),
            /*fds: r("FDS"),
            msr: r("MSR"),
            fcs: r("FCS"),*/
            fs_base: r("FS_OFFSET"),
            gs_base: r("GS_OFFSET"),
        }
    }

    pub fn get_node(&self, reg: X64Register) -> pcode::VarNode {
        match reg {
            X64Register::Rax => self.rax,
            X64Register::Rbx => self.rbx,
            X64Register::Rcx => self.rcx,
            X64Register::Rdx => self.rdx,
            X64Register::Rsi => self.rsi,
            X64Register::Rdi => self.rdi,
            X64Register::Rbp => self.rbp,
            X64Register::Rsp => self.rsp,
            X64Register::R8 => self.r8,
            X64Register::R9 => self.r9,
            X64Register::R10 => self.r10,
            X64Register::R11 => self.r11,
            X64Register::R12 => self.r12,
            X64Register::R13 => self.r13,
            X64Register::R14 => self.r14,
            X64Register::R15 => self.r15,
            X64Register::Rip => self.rip,
            X64Register::Eflags => self.eflags,
            X64Register::Cs => self.cs,
            X64Register::Ds => self.ds,
            X64Register::Es => self.es,
            X64Register::Fs => self.fs,
            X64Register::Gs => self.gs,
            X64Register::Ss => self.ss,
            X64Register::Ah => self.ah,
            X64Register::Al => self.al,
            X64Register::Ax => self.ax,
            X64Register::Bh => self.bh,
            X64Register::Bl => self.bl,
            X64Register::Bpl => self.bpl,
            X64Register::Ch => self.ch,
            X64Register::Cl => self.cl,
            X64Register::Cx => self.cx,
            X64Register::Dh => self.dh,
            X64Register::Dil => self.dil,
            X64Register::Dl => self.dl,
            X64Register::Dx => self.dx,
            X64Register::Eax => self.eax,
            X64Register::Ebp => self.ebp,
            X64Register::Ebx => self.ebx,
            X64Register::Ecx => self.ecx,
            X64Register::Edi => self.edi,
            X64Register::Edx => self.edx,
            X64Register::Esi => self.esi,
            X64Register::Esp => self.esp,
            X64Register::Fpsw => self.fpsw,
            X64Register::Gdtr => self.gdtr,
            X64Register::Idtr => self.idtr,
            X64Register::Ldtr => self.ldtr,
            X64Register::Tr => self.tr,
            X64Register::Cr0 => self.cr0,
            X64Register::Cr1 => self.cr1,
            X64Register::Cr2 => self.cr2,
            X64Register::Cr3 => self.cr3,
            X64Register::Cr4 => self.cr4,
            X64Register::Cr8 => self.cr8,
            X64Register::Dr0 => self.dr0,
            X64Register::Dr1 => self.dr1,
            X64Register::Dr2 => self.dr2,
            X64Register::Dr3 => self.dr3,
            X64Register::Dr4 => self.dr4,
            X64Register::Dr5 => self.dr5,
            X64Register::Dr6 => self.dr6,
            X64Register::Dr7 => self.dr7,
            X64Register::Fp0 => self.fp0,
            X64Register::Fp1 => self.fp1,
            X64Register::Fp2 => self.fp2,
            X64Register::Fp3 => self.fp3,
            X64Register::Fp4 => self.fp4,
            X64Register::Fp5 => self.fp5,
            X64Register::Fp6 => self.fp6,
            X64Register::Fp7 => self.fp7,
            /*X64Register::K0 => self.k0,
            X64Register::K1 => self.k1,
            X64Register::K2 => self.k2,
            X64Register::K3 => self.k3,
            X64Register::K4 => self.k4,
            X64Register::K5 => self.k5,
            X64Register::K6 => self.k6,
            X64Register::K7 => self.k7,*/
            X64Register::Mm0 => self.mm0,
            X64Register::Mm1 => self.mm1,
            X64Register::Mm2 => self.mm2,
            X64Register::Mm3 => self.mm3,
            X64Register::Mm4 => self.mm4,
            X64Register::Mm5 => self.mm5,
            X64Register::Mm6 => self.mm6,
            X64Register::Mm7 => self.mm7,
            X64Register::St0 => self.st0,
            X64Register::St1 => self.st1,
            X64Register::St2 => self.st2,
            X64Register::St3 => self.st3,
            X64Register::St4 => self.st4,
            X64Register::St5 => self.st5,
            X64Register::St6 => self.st6,
            X64Register::St7 => self.st7,
            X64Register::Xmm0 => self.xmm0,
            X64Register::Xmm1 => self.xmm1,
            X64Register::Xmm2 => self.xmm2,
            X64Register::Xmm3 => self.xmm3,
            X64Register::Xmm4 => self.xmm4,
            X64Register::Xmm5 => self.xmm5,
            X64Register::Xmm6 => self.xmm6,
            X64Register::Xmm7 => self.xmm7,
            X64Register::Xmm8 => self.xmm8,
            X64Register::Xmm9 => self.xmm9,
            X64Register::Xmm10 => self.xmm10,
            X64Register::Xmm11 => self.xmm11,
            X64Register::Xmm12 => self.xmm12,
            X64Register::Xmm13 => self.xmm13,
            X64Register::Xmm14 => self.xmm14,
            X64Register::Xmm15 => self.xmm15,
            /*X64Register::Xmm16 => self.xmm16,
            X64Register::Xmm17 => self.xmm17,
            X64Register::Xmm18 => self.xmm18,
            X64Register::Xmm19 => self.xmm19,
            X64Register::Xmm20 => self.xmm20,
            X64Register::Xmm21 => self.xmm21,
            X64Register::Xmm22 => self.xmm22,
            X64Register::Xmm23 => self.xmm23,
            X64Register::Xmm24 => self.xmm24,
            X64Register::Xmm25 => self.xmm25,
            X64Register::Xmm26 => self.xmm26,
            X64Register::Xmm27 => self.xmm27,
            X64Register::Xmm28 => self.xmm28,
            X64Register::Xmm29 => self.xmm29,
            X64Register::Xmm30 => self.xmm30,
            X64Register::Xmm31 => self.xmm31,*/
            X64Register::Ymm0 => self.ymm0,
            X64Register::Ymm1 => self.ymm1,
            X64Register::Ymm2 => self.ymm2,
            X64Register::Ymm3 => self.ymm3,
            X64Register::Ymm4 => self.ymm4,
            X64Register::Ymm5 => self.ymm5,
            X64Register::Ymm6 => self.ymm6,
            X64Register::Ymm7 => self.ymm7,
            X64Register::Ymm8 => self.ymm8,
            X64Register::Ymm9 => self.ymm9,
            X64Register::Ymm10 => self.ymm10,
            X64Register::Ymm11 => self.ymm11,
            X64Register::Ymm12 => self.ymm12,
            X64Register::Ymm13 => self.ymm13,
            X64Register::Ymm14 => self.ymm14,
            X64Register::Ymm15 => self.ymm15,
            /*X64Register::Ymm16 => self.ymm16,
            X64Register::Ymm17 => self.ymm17,
            X64Register::Ymm18 => self.ymm18,
            X64Register::Ymm19 => self.ymm19,
            X64Register::Ymm20 => self.ymm20,
            X64Register::Ymm21 => self.ymm21,
            X64Register::Ymm22 => self.ymm22,
            X64Register::Ymm23 => self.ymm23,
            X64Register::Ymm24 => self.ymm24,
            X64Register::Ymm25 => self.ymm25,
            X64Register::Ymm26 => self.ymm26,
            X64Register::Ymm27 => self.ymm27,
            X64Register::Ymm28 => self.ymm28,
            X64Register::Ymm29 => self.ymm29,
            X64Register::Ymm30 => self.ymm30,
            X64Register::Ymm31 => self.ymm31,*/
            /*X64Register::Zmm0 => self.zmm0,
            X64Register::Zmm1 => self.zmm1,
            X64Register::Zmm2 => self.zmm2,
            X64Register::Zmm3 => self.zmm3,
            X64Register::Zmm4 => self.zmm4,
            X64Register::Zmm5 => self.zmm5,
            X64Register::Zmm6 => self.zmm6,
            X64Register::Zmm7 => self.zmm7,
            X64Register::Zmm8 => self.zmm8,
            X64Register::Zmm9 => self.zmm9,
            X64Register::Zmm10 => self.zmm10,
            X64Register::Zmm11 => self.zmm11,
            X64Register::Zmm12 => self.zmm12,
            X64Register::Zmm13 => self.zmm13,
            X64Register::Zmm14 => self.zmm14,
            X64Register::Zmm15 => self.zmm15,
            X64Register::Zmm16 => self.zmm16,
            X64Register::Zmm17 => self.zmm17,
            X64Register::Zmm18 => self.zmm18,
            X64Register::Zmm19 => self.zmm19,
            X64Register::Zmm20 => self.zmm20,
            X64Register::Zmm21 => self.zmm21,
            X64Register::Zmm22 => self.zmm22,
            X64Register::Zmm23 => self.zmm23,
            X64Register::Zmm24 => self.zmm24,
            X64Register::Zmm25 => self.zmm25,
            X64Register::Zmm26 => self.zmm26,
            X64Register::Zmm27 => self.zmm27,
            X64Register::Zmm28 => self.zmm28,
            X64Register::Zmm29 => self.zmm29,
            X64Register::Zmm30 => self.zmm30,
            X64Register::Zmm31 => self.zmm31,*/
            X64Register::R8b => self.r8b,
            X64Register::R9b => self.r9b,
            X64Register::R10b => self.r10b,
            X64Register::R11b => self.r11b,
            X64Register::R12b => self.r12b,
            X64Register::R13b => self.r13b,
            X64Register::R14b => self.r14b,
            X64Register::R15b => self.r15b,
            X64Register::R8d => self.r8d,
            X64Register::R9d => self.r9d,
            X64Register::R10d => self.r10d,
            X64Register::R11d => self.r11d,
            X64Register::R12d => self.r12d,
            X64Register::R13d => self.r13d,
            X64Register::R14d => self.r14d,
            X64Register::R15d => self.r15d,
            X64Register::R8w => self.r8w,
            X64Register::R9w => self.r9w,
            X64Register::R10w => self.r10w,
            X64Register::R11w => self.r11w,
            X64Register::R12w => self.r12w,
            X64Register::R13w => self.r13w,
            X64Register::R14w => self.r14w,
            X64Register::R15w => self.r15w,
            X64Register::Fpcw => self.fpcw,
            X64Register::Fptag => self.fptag,
            //X64Register::Msr => self.msr,
            X64Register::Mxcsr => self.mxcsr,
            X64Register::FsBase => self.fs_base,
            X64Register::GsBase => self.gs_base,
            X64Register::Flags => self.flags,
            X64Register::Rflags => self.rflags,
            X64Register::Fip => self.fip,
            //X64Register::Fcs => self.fcs,
            X64Register::Fdp => self.fdp,
            //X64Register::Fds => self.fds,
            X64Register::Fop => self.fop,
            _ => panic!("Unsupported register"),
        }
    }
}
