function calc(obj, n) {
    for (const k in obj) {
        if (obj[k] && n || k) n++;
    }
    do { n--; } while (n > 0);
    return n;
}
