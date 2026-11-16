import type { CommandReading } from './types';

/** Whether the daemon recognised any command; a reading with none adds nothing over the script. */
export function recognisesAny(reading: CommandReading | null | undefined): reading is CommandReading {
  return !!reading && reading.steps.some((step) => step.intent.kind !== 'unknown');
}
