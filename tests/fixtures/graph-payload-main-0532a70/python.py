class Item:
    def run(self, flag):
        callback = lambda n: consume(n)
        if flag:
            for n in range(2):
                callback(n)

def consume(n):
    return n
