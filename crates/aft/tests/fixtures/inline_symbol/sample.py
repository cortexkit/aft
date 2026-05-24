BASE_URL = "https://api.example.com"

def add(a, b):
    total = a + b
    return total

def main():
    x = 10
    y = 20
    result = add(x, y)
    print(result)

def double(n):
    return n * 2

def caller():
    val = double(5)
    print(val)
