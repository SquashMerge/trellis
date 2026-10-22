class Item {
    int value;
    int getValue() { return value; }
    void setValue(int next) { value = next; }
    void run(boolean flag) {
        if (flag) {
            for (int n = 0; n < 2; n++) {
                consume(n);
            }
        }
    }
    void consume(int n) {}
}
