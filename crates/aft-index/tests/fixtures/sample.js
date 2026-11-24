// Regular function
export function multiply(a, b) {
  return a * b;
}

// Arrow function assigned to const
export const divide = (a, b) => {
  if (b === 0) throw new Error("division by zero");
  return a / b;
};

// Class with methods
class EventEmitter {
  constructor() {
    this.listeners = {};
  }

  on(event, callback) {
    if (!this.listeners[event]) {
      this.listeners[event] = [];
    }
    this.listeners[event].push(callback);
  }

  emit(event, data) {
    const callbacks = this.listeners[event] || [];
    callbacks.forEach((cb) => cb(data));
  }
}

// Default export
export default function main() {
  const emitter = new EventEmitter();
  emitter.on("test", (data) => console.log(data));
  emitter.emit("test", "hello");
}

// Non-exported arrow fn
const internalUtil = () => {
  return 42;
};
