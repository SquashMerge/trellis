function calc(x, y) {
    try {
        if (x && y) return x;
        switch (x) {
            case 1: return 1;
            default: return 0;
        }
    } catch (e) {
        throw e;
    }
}
