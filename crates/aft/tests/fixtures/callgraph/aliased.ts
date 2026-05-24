import { validate as checker } from './helpers';

export function runCheck(data: string): boolean {
    return checker(data);
}
