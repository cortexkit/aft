import React from "react";

// Exported interface
export interface ButtonProps {
  label: string;
  onClick: () => void;
  disabled?: boolean;
}

// React component as arrow function
export const Button: React.FC<ButtonProps> = ({ label, onClick, disabled }) => {
  return (
    <button onClick={onClick} disabled={disabled}>
      {label}
    </button>
  );
};

// Class component
export class Counter extends React.Component {
  state = { count: 0 };

  increment() {
    this.setState({ count: this.state.count + 1 });
  }

  render() {
    return <div>{this.state.count}</div>;
  }
}

// Regular function
export function formatLabel(text: string): string {
  return text.toUpperCase();
}
