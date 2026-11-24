import { processInput } from './data_processor';

export function transformData(rawInput: string): string {
    const cleaned = rawInput;
    const result = processInput(cleaned);
    return result;
}

export function complexFlow(data: string): void {
    const { name, value } = JSON.parse(data);
    console.log(name, value);
}
