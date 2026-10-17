class Example {
    int calc(int x, boolean y) {
        try {
            if (y && x > 0) return x;
            switch (x) {
                case 1: return 1;
                default: return 0;
            }
        } catch (RuntimeException e) {
            throw e;
        }
    }
}
