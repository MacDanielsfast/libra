module M {
    resource struct R {f: u64}

    t0() {
        let (x, x);
        let (f, R{f}, f);
        x = 0;
        f = 0;
    }
}
