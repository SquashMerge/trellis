function make() { return function done() {}; }
function call_chain() {
    make()();
}
