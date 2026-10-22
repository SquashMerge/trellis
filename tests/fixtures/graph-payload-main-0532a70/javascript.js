class Item {
    get value() { return this.raw; }
    set value(next) { this.raw = next; }
    run(flag) {
        const callback = () => helper();
        if (flag) {
            while (flag) {
                callback(); helper();
                break;
            }
        }
    }
}
function helper() {}
