class Controls {
    int calc(int[] xs, int n) {
        for (int x : xs) {
            if (x > 0 && n > 0 || n < 0) n++;
        }
        do { n--; } while (n > 0);
        return switch (n) { case 1 -> 1; default -> 0; };
    }
}
