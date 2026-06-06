# TC39 Temporal Proposal — Specification

> Plain-text/Markdown rendering of the official ecmarkup specification at
> <https://tc39.es/proposal-temporal/>, retrieved 2026-06-06.
>
> Stage 4 (to be merged into ECMA-262 / ECMA-402). Converted from the rendered
> single-page HTML; section numbering, abstract operations, normative algorithm
> steps, and the BSD license (at the end) are preserved. This is a derived
> convenience copy — the URL above is authoritative.

Table of Contents

1. Introduction
2. +1 The Temporal Object
   1. +1.1 Value Properties of the Temporal Object
      1. 1.1.1 Temporal [ %Symbol.toStringTag% ]
   2. +1.2 Constructor Properties of the Temporal Object
      1. 1.2.1 Temporal.Instant ( . . . )
      2. 1.2.2 Temporal.PlainDateTime ( . . . )
      3. 1.2.3 Temporal.PlainDate ( . . . )
      4. 1.2.4 Temporal.PlainTime ( . . . )
      5. 1.2.5 Temporal.PlainYearMonth ( . . . )
      6. 1.2.6 Temporal.PlainMonthDay ( . . . )
      7. 1.2.7 Temporal.Duration ( . . . )
      8. 1.2.8 Temporal.ZonedDateTime ( . . . )
   3. +1.3 Other Properties of the Temporal Object
      1. 1.3.1 Temporal.Now
3. +2 The Temporal.Now Object
   1. +2.1 Value Properties of the Temporal.Now Object
      1. 2.1.1 Temporal.Now [ %Symbol.toStringTag% ]
   2. +2.2 Function Properties of the Temporal.Now Object
      1. 2.2.1 Temporal.Now.timeZoneId ( )
      2. 2.2.2 Temporal.Now.instant ( )
      3. 2.2.3 Temporal.Now.plainDateTimeISO ( [ temporalTimeZoneLike ] )
      4. 2.2.4 Temporal.Now.zonedDateTimeISO ( [ temporalTimeZoneLike ] )
      5. 2.2.5 Temporal.Now.plainDateISO ( [ temporalTimeZoneLike ] )
      6. 2.2.6 Temporal.Now.plainTimeISO ( [ temporalTimeZoneLike ] )
   3. +2.3 Abstract Operations
      1. 2.3.1 HostSystemUTCEpochNanoseconds ( global )
      2. 2.3.2 SystemUTCEpochMilliseconds ( )
      3. 2.3.3 SystemUTCEpochNanoseconds ( )
      4. 2.3.4 SystemDateTime ( temporalTimeZoneLike )
4. +3 Temporal.PlainDate Objects
   1. +3.1 The Temporal.PlainDate Constructor
      1. 3.1.1 Temporal.PlainDate ( isoYear, isoMonth, isoDay [ , calendar ] )
   2. +3.2 Properties of the Temporal.PlainDate Constructor
      1. 3.2.1 Temporal.PlainDate.prototype
      2. 3.2.2 Temporal.PlainDate.from ( item [ , options ] )
      3. 3.2.3 Temporal.PlainDate.compare ( one, two )
   3. +3.3 Properties of the Temporal.PlainDate Prototype Object
      1. 3.3.1 Temporal.PlainDate.prototype.constructor
      2. 3.3.2 Temporal.PlainDate.prototype[ %Symbol.toStringTag% ]
      3. 3.3.3 get Temporal.PlainDate.prototype.calendarId
      4. 3.3.4 get Temporal.PlainDate.prototype.era
      5. 3.3.5 get Temporal.PlainDate.prototype.eraYear
      6. 3.3.6 get Temporal.PlainDate.prototype.year
      7. 3.3.7 get Temporal.PlainDate.prototype.month
      8. 3.3.8 get Temporal.PlainDate.prototype.monthCode
      9. 3.3.9 get Temporal.PlainDate.prototype.day
      10. 3.3.10 get Temporal.PlainDate.prototype.dayOfWeek
      11. 3.3.11 get Temporal.PlainDate.prototype.dayOfYear
      12. 3.3.12 get Temporal.PlainDate.prototype.weekOfYear
      13. 3.3.13 get Temporal.PlainDate.prototype.yearOfWeek
      14. 3.3.14 get Temporal.PlainDate.prototype.daysInWeek
      15. 3.3.15 get Temporal.PlainDate.prototype.daysInMonth
      16. 3.3.16 get Temporal.PlainDate.prototype.daysInYear
      17. 3.3.17 get Temporal.PlainDate.prototype.monthsInYear
      18. 3.3.18 get Temporal.PlainDate.prototype.inLeapYear
      19. 3.3.19 Temporal.PlainDate.prototype.toPlainYearMonth ( )
      20. 3.3.20 Temporal.PlainDate.prototype.toPlainMonthDay ( )
      21. 3.3.21 Temporal.PlainDate.prototype.add ( temporalDurationLike [ , options ] )
      22. 3.3.22 Temporal.PlainDate.prototype.subtract ( temporalDurationLike [ , options ] )
      23. 3.3.23 Temporal.PlainDate.prototype.with ( temporalDateLike [ , options ] )
      24. 3.3.24 Temporal.PlainDate.prototype.withCalendar ( calendarLike )
      25. 3.3.25 Temporal.PlainDate.prototype.until ( other [ , options ] )
      26. 3.3.26 Temporal.PlainDate.prototype.since ( other [ , options ] )
      27. 3.3.27 Temporal.PlainDate.prototype.equals ( other )
      28. 3.3.28 Temporal.PlainDate.prototype.toPlainDateTime ( [ temporalTime ] )
      29. 3.3.29 Temporal.PlainDate.prototype.toZonedDateTime ( item )
      30. 3.3.30 Temporal.PlainDate.prototype.toString ( [ options ] )
      31. 3.3.31 Temporal.PlainDate.prototype.toLocaleString ( [ locales [ , options ] ] )
      32. 3.3.32 Temporal.PlainDate.prototype.toJSON ( )
      33. 3.3.33 Temporal.PlainDate.prototype.valueOf ( )
   4. 3.4 Properties of Temporal.PlainDate Instances
   5. +3.5 Abstract Operations for Temporal.PlainDate Objects
      1. 3.5.1 ISO Date Records
      2. 3.5.2 CreateISODateRecord ( year, month, day )
      3. 3.5.3 CreateTemporalDate ( isoDate, calendar [ , newTarget ] )
      4. 3.5.4 ToTemporalDate ( item [ , options ] )
      5. 3.5.5 CompareSurpasses ( sign, year, monthOrCode, day, target )
      6. 3.5.6 ISODateSurpasses ( sign, baseDate, isoDate2, years, months, weeks, days )
      7. 3.5.7 RegulateISODate ( year, month, day, overflow )
      8. 3.5.8 IsValidISODate ( year, month, day )
      9. 3.5.9 AddDaysToISODate ( isoDate, days )
      10. 3.5.10 PadISOYear ( y )
      11. 3.5.11 TemporalDateToString ( temporalDate, showCalendar )
      12. 3.5.12 ISODateWithinLimits ( isoDate )
      13. 3.5.13 CompareISODate ( isoDate1, isoDate2 )
      14. 3.5.14 DifferenceTemporalPlainDate ( operation, temporalDate, other, options )
      15. 3.5.15 AddDurationToDate ( operation, temporalDate, temporalDurationLike, options )
5. +4 Temporal.PlainTime Objects
   1. +4.1 The Temporal.PlainTime Constructor
      1. 4.1.1 Temporal.PlainTime ( [ hour [ , minute [ , second [ , millisecond [ , microsecond [ , nanosecond ] ] ] ] ] ] )
   2. +4.2 Properties of the Temporal.PlainTime Constructor
      1. 4.2.1 Temporal.PlainTime.prototype
      2. 4.2.2 Temporal.PlainTime.from ( item [ , options ] )
      3. 4.2.3 Temporal.PlainTime.compare ( one, two )
   3. +4.3 Properties of the Temporal.PlainTime Prototype Object
      1. 4.3.1 Temporal.PlainTime.prototype.constructor
      2. 4.3.2 Temporal.PlainTime.prototype[ %Symbol.toStringTag% ]
      3. 4.3.3 get Temporal.PlainTime.prototype.hour
      4. 4.3.4 get Temporal.PlainTime.prototype.minute
      5. 4.3.5 get Temporal.PlainTime.prototype.second
      6. 4.3.6 get Temporal.PlainTime.prototype.millisecond
      7. 4.3.7 get Temporal.PlainTime.prototype.microsecond
      8. 4.3.8 get Temporal.PlainTime.prototype.nanosecond
      9. 4.3.9 Temporal.PlainTime.prototype.add ( temporalDurationLike )
      10. 4.3.10 Temporal.PlainTime.prototype.subtract ( temporalDurationLike )
      11. 4.3.11 Temporal.PlainTime.prototype.with ( temporalTimeLike [ , options ] )
      12. 4.3.12 Temporal.PlainTime.prototype.until ( other [ , options ] )
      13. 4.3.13 Temporal.PlainTime.prototype.since ( other [ , options ] )
      14. 4.3.14 Temporal.PlainTime.prototype.round ( roundTo )
      15. 4.3.15 Temporal.PlainTime.prototype.equals ( other )
      16. 4.3.16 Temporal.PlainTime.prototype.toString ( [ options ] )
      17. 4.3.17 Temporal.PlainTime.prototype.toLocaleString ( [ locales [ , options ] ] )
      18. 4.3.18 Temporal.PlainTime.prototype.toJSON ( )
      19. 4.3.19 Temporal.PlainTime.prototype.valueOf ( )
   4. 4.4 Properties of Temporal.PlainTime Instances
   5. +4.5 Abstract Operations
      1. 4.5.1 Time Records
      2. 4.5.2 CreateTimeRecord ( hour, minute, second, millisecond, microsecond, nanosecond [ , deltaDays ] )
      3. 4.5.3 MidnightTimeRecord ( )
      4. 4.5.4 NoonTimeRecord ( )
      5. 4.5.5 DifferenceTime ( time1, time2 )
      6. 4.5.6 ToTemporalTime ( item [ , options ] )
      7. 4.5.7 ToTimeRecordOrMidnight ( item )
      8. 4.5.8 RegulateTime ( hour, minute, second, millisecond, microsecond, nanosecond, overflow )
      9. 4.5.9 IsValidTime ( hour, minute, second, millisecond, microsecond, nanosecond )
      10. 4.5.10 BalanceTime ( hour, minute, second, millisecond, microsecond, nanosecond )
      11. 4.5.11 CreateTemporalTime ( time [ , newTarget ] )
      12. 4.5.12 ToTemporalTimeRecord ( temporalTimeLike [ , completeness ] )
      13. 4.5.13 TimeRecordToString ( time, precision )
      14. 4.5.14 CompareTimeRecord ( time1, time2 )
      15. 4.5.15 AddTime ( time, timeDuration )
      16. 4.5.16 RoundTime ( time, increment, unit, roundingMode )
      17. 4.5.17 DifferenceTemporalPlainTime ( operation, temporalTime, other, options )
      18. 4.5.18 AddDurationToTime ( operation, temporalTime, temporalDurationLike )
6. +5 Temporal.PlainDateTime Objects
   1. +5.1 The Temporal.PlainDateTime Constructor
      1. 5.1.1 Temporal.PlainDateTime ( isoYear, isoMonth, isoDay [ , hour [ , minute [ , second [ , millisecond [ , microsecond [ , nanosecond [ , calendar ] ] ] ] ] ] ] )
   2. +5.2 Properties of the Temporal.PlainDateTime Constructor
      1. 5.2.1 Temporal.PlainDateTime.prototype
      2. 5.2.2 Temporal.PlainDateTime.from ( item [ , options ] )
      3. 5.2.3 Temporal.PlainDateTime.compare ( one, two )
   3. +5.3 Properties of the Temporal.PlainDateTime Prototype Object
      1. 5.3.1 Temporal.PlainDateTime.prototype.constructor
      2. 5.3.2 Temporal.PlainDateTime.prototype[ %Symbol.toStringTag% ]
      3. 5.3.3 get Temporal.PlainDateTime.prototype.calendarId
      4. 5.3.4 get Temporal.PlainDateTime.prototype.era
      5. 5.3.5 get Temporal.PlainDateTime.prototype.eraYear
      6. 5.3.6 get Temporal.PlainDateTime.prototype.year
      7. 5.3.7 get Temporal.PlainDateTime.prototype.month
      8. 5.3.8 get Temporal.PlainDateTime.prototype.monthCode
      9. 5.3.9 get Temporal.PlainDateTime.prototype.day
      10. 5.3.10 get Temporal.PlainDateTime.prototype.hour
      11. 5.3.11 get Temporal.PlainDateTime.prototype.minute
      12. 5.3.12 get Temporal.PlainDateTime.prototype.second
      13. 5.3.13 get Temporal.PlainDateTime.prototype.millisecond
      14. 5.3.14 get Temporal.PlainDateTime.prototype.microsecond
      15. 5.3.15 get Temporal.PlainDateTime.prototype.nanosecond
      16. 5.3.16 get Temporal.PlainDateTime.prototype.dayOfWeek
      17. 5.3.17 get Temporal.PlainDateTime.prototype.dayOfYear
      18. 5.3.18 get Temporal.PlainDateTime.prototype.weekOfYear
      19. 5.3.19 get Temporal.PlainDateTime.prototype.yearOfWeek
      20. 5.3.20 get Temporal.PlainDateTime.prototype.daysInWeek
      21. 5.3.21 get Temporal.PlainDateTime.prototype.daysInMonth
      22. 5.3.22 get Temporal.PlainDateTime.prototype.daysInYear
      23. 5.3.23 get Temporal.PlainDateTime.prototype.monthsInYear
      24. 5.3.24 get Temporal.PlainDateTime.prototype.inLeapYear
      25. 5.3.25 Temporal.PlainDateTime.prototype.with ( temporalDateTimeLike [ , options ] )
      26. 5.3.26 Temporal.PlainDateTime.prototype.withPlainTime ( [ plainTimeLike ] )
      27. 5.3.27 Temporal.PlainDateTime.prototype.withCalendar ( calendarLike )
      28. 5.3.28 Temporal.PlainDateTime.prototype.add ( temporalDurationLike [ , options ] )
      29. 5.3.29 Temporal.PlainDateTime.prototype.subtract ( temporalDurationLike [ , options ] )
      30. 5.3.30 Temporal.PlainDateTime.prototype.until ( other [ , options ] )
      31. 5.3.31 Temporal.PlainDateTime.prototype.since ( other [ , options ] )
      32. 5.3.32 Temporal.PlainDateTime.prototype.round ( roundTo )
      33. 5.3.33 Temporal.PlainDateTime.prototype.equals ( other )
      34. 5.3.34 Temporal.PlainDateTime.prototype.toString ( [ options ] )
      35. 5.3.35 Temporal.PlainDateTime.prototype.toLocaleString ( [ locales [ , options ] ] )
      36. 5.3.36 Temporal.PlainDateTime.prototype.toJSON ( )
      37. 5.3.37 Temporal.PlainDateTime.prototype.valueOf ( )
      38. 5.3.38 Temporal.PlainDateTime.prototype.toZonedDateTime ( temporalTimeZoneLike [ , options ] )
      39. 5.3.39 Temporal.PlainDateTime.prototype.toPlainDate ( )
      40. 5.3.40 Temporal.PlainDateTime.prototype.toPlainTime ( )
   4. 5.4 Properties of Temporal.PlainDateTime Instances
   5. +5.5 Abstract Operations
      1. 5.5.1 ISO Date-Time Records
      2. 5.5.2 TimeValueToISODateTimeRecord ( t )
      3. 5.5.3 CombineISODateAndTimeRecord ( isoDate, time )
      4. 5.5.4 ISODateTimeWithinLimits ( isoDateTime )
      5. 5.5.5 InterpretTemporalDateTimeFields ( calendar, fields, overflow )
      6. 5.5.6 ToTemporalDateTime ( item [ , options ] )
      7. 5.5.7 BalanceISODateTime ( year, month, day, hour, minute, second, millisecond, microsecond, nanosecond )
      8. 5.5.8 CreateTemporalDateTime ( isoDateTime, calendar [ , newTarget ] )
      9. 5.5.9 ISODateTimeToString ( isoDateTime, calendar, precision, showCalendar )
      10. 5.5.10 CompareISODateTime ( isoDateTime1, isoDateTime2 )
      11. 5.5.11 RoundISODateTime ( isoDateTime, increment, unit, roundingMode )
      12. 5.5.12 DifferenceISODateTime ( isoDateTime1, isoDateTime2, calendar, largestUnit )
      13. 5.5.13 DifferencePlainDateTimeWithRounding ( isoDateTime1, isoDateTime2, calendar, largestUnit, roundingIncrement, smallestUnit, roundingMode )
      14. 5.5.14 DifferencePlainDateTimeWithTotal ( isoDateTime1, isoDateTime2, calendar, unit )
      15. 5.5.15 DifferenceTemporalPlainDateTime ( operation, dateTime, other, options )
      16. 5.5.16 AddDurationToDateTime ( operation, dateTime, temporalDurationLike, options )
7. +6 Temporal.ZonedDateTime Objects
   1. +6.1 The Temporal.ZonedDateTime Constructor
      1. 6.1.1 Temporal.ZonedDateTime ( epochNanoseconds, timeZone [ , calendar ] )
   2. +6.2 Properties of the Temporal.ZonedDateTime Constructor
      1. 6.2.1 Temporal.ZonedDateTime.prototype
      2. 6.2.2 Temporal.ZonedDateTime.from ( item [ , options ] )
      3. 6.2.3 Temporal.ZonedDateTime.compare ( one, two )
   3. +6.3 Properties of the Temporal.ZonedDateTime Prototype Object
      1. 6.3.1 Temporal.ZonedDateTime.prototype.constructor
      2. 6.3.2 Temporal.ZonedDateTime.prototype[ %Symbol.toStringTag% ]
      3. 6.3.3 get Temporal.ZonedDateTime.prototype.calendarId
      4. 6.3.4 get Temporal.ZonedDateTime.prototype.timeZoneId
      5. 6.3.5 get Temporal.ZonedDateTime.prototype.era
      6. 6.3.6 get Temporal.ZonedDateTime.prototype.eraYear
      7. 6.3.7 get Temporal.ZonedDateTime.prototype.year
      8. 6.3.8 get Temporal.ZonedDateTime.prototype.month
      9. 6.3.9 get Temporal.ZonedDateTime.prototype.monthCode
      10. 6.3.10 get Temporal.ZonedDateTime.prototype.day
      11. 6.3.11 get Temporal.ZonedDateTime.prototype.hour
      12. 6.3.12 get Temporal.ZonedDateTime.prototype.minute
      13. 6.3.13 get Temporal.ZonedDateTime.prototype.second
      14. 6.3.14 get Temporal.ZonedDateTime.prototype.millisecond
      15. 6.3.15 get Temporal.ZonedDateTime.prototype.microsecond
      16. 6.3.16 get Temporal.ZonedDateTime.prototype.nanosecond
      17. 6.3.17 get Temporal.ZonedDateTime.prototype.epochMilliseconds
      18. 6.3.18 get Temporal.ZonedDateTime.prototype.epochNanoseconds
      19. 6.3.19 get Temporal.ZonedDateTime.prototype.dayOfWeek
      20. 6.3.20 get Temporal.ZonedDateTime.prototype.dayOfYear
      21. 6.3.21 get Temporal.ZonedDateTime.prototype.weekOfYear
      22. 6.3.22 get Temporal.ZonedDateTime.prototype.yearOfWeek
      23. 6.3.23 get Temporal.ZonedDateTime.prototype.hoursInDay
      24. 6.3.24 get Temporal.ZonedDateTime.prototype.daysInWeek
      25. 6.3.25 get Temporal.ZonedDateTime.prototype.daysInMonth
      26. 6.3.26 get Temporal.ZonedDateTime.prototype.daysInYear
      27. 6.3.27 get Temporal.ZonedDateTime.prototype.monthsInYear
      28. 6.3.28 get Temporal.ZonedDateTime.prototype.inLeapYear
      29. 6.3.29 get Temporal.ZonedDateTime.prototype.offsetNanoseconds
      30. 6.3.30 get Temporal.ZonedDateTime.prototype.offset
      31. 6.3.31 Temporal.ZonedDateTime.prototype.with ( temporalZonedDateTimeLike [ , options ] )
      32. 6.3.32 Temporal.ZonedDateTime.prototype.withPlainTime ( [ plainTimeLike ] )
      33. 6.3.33 Temporal.ZonedDateTime.prototype.withTimeZone ( timeZoneLike )
      34. 6.3.34 Temporal.ZonedDateTime.prototype.withCalendar ( calendarLike )
      35. 6.3.35 Temporal.ZonedDateTime.prototype.add ( temporalDurationLike [ , options ] )
      36. 6.3.36 Temporal.ZonedDateTime.prototype.subtract ( temporalDurationLike [ , options ] )
      37. 6.3.37 Temporal.ZonedDateTime.prototype.until ( other [ , options ] )
      38. 6.3.38 Temporal.ZonedDateTime.prototype.since ( other [ , options ] )
      39. 6.3.39 Temporal.ZonedDateTime.prototype.round ( roundTo )
      40. 6.3.40 Temporal.ZonedDateTime.prototype.equals ( other )
      41. 6.3.41 Temporal.ZonedDateTime.prototype.toString ( [ options ] )
      42. 6.3.42 Temporal.ZonedDateTime.prototype.toLocaleString ( [ locales [ , options ] ] )
      43. 6.3.43 Temporal.ZonedDateTime.prototype.toJSON ( )
      44. 6.3.44 Temporal.ZonedDateTime.prototype.valueOf ( )
      45. 6.3.45 Temporal.ZonedDateTime.prototype.startOfDay ( )
      46. 6.3.46 Temporal.ZonedDateTime.prototype.getTimeZoneTransition ( directionParam )
      47. 6.3.47 Temporal.ZonedDateTime.prototype.toInstant ( )
      48. 6.3.48 Temporal.ZonedDateTime.prototype.toPlainDate ( )
      49. 6.3.49 Temporal.ZonedDateTime.prototype.toPlainTime ( )
      50. 6.3.50 Temporal.ZonedDateTime.prototype.toPlainDateTime ( )
   4. 6.4 Properties of Temporal.ZonedDateTime Instances
   5. +6.5 Abstract Operations
      1. 6.5.1 InterpretISODateTimeOffset ( isoDate, time, offsetBehaviour, offsetNanoseconds, timeZone, disambiguation, offsetOption, matchBehaviour )
      2. 6.5.2 ToTemporalZonedDateTime ( item [ , options ] )
      3. 6.5.3 CreateTemporalZonedDateTime ( epochNanoseconds, timeZone, calendar [ , newTarget ] )
      4. 6.5.4 TemporalZonedDateTimeToString ( zonedDateTime, precision, showCalendar, showTimeZone, showOffset [ , increment [ , unit [ , roundingMode ] ] ] )
      5. 6.5.5 AddZonedDateTime ( epochNanoseconds, timeZone, calendar, duration, overflow )
      6. 6.5.6 DifferenceZonedDateTime ( ns1, ns2, timeZone, calendar, largestUnit )
      7. 6.5.7 DifferenceZonedDateTimeWithRounding ( ns1, ns2, timeZone, calendar, largestUnit, roundingIncrement, smallestUnit, roundingMode )
      8. 6.5.8 DifferenceZonedDateTimeWithTotal ( ns1, ns2, timeZone, calendar, unit )
      9. 6.5.9 DifferenceTemporalZonedDateTime ( operation, zonedDateTime, other, options )
      10. 6.5.10 AddDurationToZonedDateTime ( operation, zonedDateTime, temporalDurationLike, options )
8. +7 Temporal.Duration Objects
   1. +7.1 The Temporal.Duration Constructor
      1. 7.1.1 Temporal.Duration ( [ years [ , months [ , weeks [ , days [ , hours [ , minutes [ , seconds [ , milliseconds [ , microseconds [ , nanoseconds ] ] ] ] ] ] ] ] ] ] )
   2. +7.2 Properties of the Temporal.Duration Constructor
      1. 7.2.1 Temporal.Duration.prototype
      2. 7.2.2 Temporal.Duration.from ( item )
      3. 7.2.3 Temporal.Duration.compare ( one, two [ , options ] )
   3. +7.3 Properties of the Temporal.Duration Prototype Object
      1. 7.3.1 Temporal.Duration.prototype.constructor
      2. 7.3.2 Temporal.Duration.prototype[ %Symbol.toStringTag% ]
      3. 7.3.3 get Temporal.Duration.prototype.years
      4. 7.3.4 get Temporal.Duration.prototype.months
      5. 7.3.5 get Temporal.Duration.prototype.weeks
      6. 7.3.6 get Temporal.Duration.prototype.days
      7. 7.3.7 get Temporal.Duration.prototype.hours
      8. 7.3.8 get Temporal.Duration.prototype.minutes
      9. 7.3.9 get Temporal.Duration.prototype.seconds
      10. 7.3.10 get Temporal.Duration.prototype.milliseconds
      11. 7.3.11 get Temporal.Duration.prototype.microseconds
      12. 7.3.12 get Temporal.Duration.prototype.nanoseconds
      13. 7.3.13 get Temporal.Duration.prototype.sign
      14. 7.3.14 get Temporal.Duration.prototype.blank
      15. 7.3.15 Temporal.Duration.prototype.with ( temporalDurationLike )
      16. 7.3.16 Temporal.Duration.prototype.negated ( )
      17. 7.3.17 Temporal.Duration.prototype.abs ( )
      18. 7.3.18 Temporal.Duration.prototype.add ( other )
      19. 7.3.19 Temporal.Duration.prototype.subtract ( other )
      20. 7.3.20 Temporal.Duration.prototype.round ( roundTo )
      21. 7.3.21 Temporal.Duration.prototype.total ( totalOf )
      22. 7.3.22 Temporal.Duration.prototype.toString ( [ options ] )
      23. 7.3.23 Temporal.Duration.prototype.toJSON ( )
      24. 7.3.24 Temporal.Duration.prototype.toLocaleString ( [ locales [ , options ] ] )
      25. 7.3.25 Temporal.Duration.prototype.valueOf ( )
   4. 7.4 Properties of Temporal.Duration Instances
   5. +7.5 Abstract Operations
      1. 7.5.1 Date Duration Records
      2. 7.5.2 Partial Duration Records
      3. 7.5.3 Internal Duration Records
      4. 7.5.4 ZeroDateDuration ( )
      5. 7.5.5 ToInternalDurationRecord ( duration )
      6. 7.5.6 ToInternalDurationRecordWith24HourDays ( duration )
      7. 7.5.7 ToDateDurationRecordWithoutTime ( duration )
      8. 7.5.8 TemporalDurationFromInternal ( internalDuration, largestUnit )
      9. 7.5.9 CreateDateDurationRecord ( years, months, weeks, days )
      10. 7.5.10 AdjustDateDurationRecord ( dateDuration, days [ , weeks [ , months ] ] )
      11. 7.5.11 CombineDateAndTimeDuration ( dateDuration, timeDuration )
      12. 7.5.12 ToTemporalDuration ( item )
      13. 7.5.13 DurationSign ( duration )
      14. 7.5.14 DateDurationSign ( dateDuration )
      15. 7.5.15 InternalDurationSign ( internalDuration )
      16. 7.5.16 IsValidDuration ( years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds )
      17. 7.5.17 DefaultTemporalLargestUnit ( duration )
      18. 7.5.18 ToTemporalPartialDurationRecord ( temporalDurationLike )
      19. 7.5.19 CreateTemporalDuration ( years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds [ , newTarget ] )
      20. 7.5.20 CreateNegatedTemporalDuration ( duration )
      21. 7.5.21 TimeDurationFromComponents ( hours, minutes, seconds, milliseconds, microseconds, nanoseconds )
      22. 7.5.22 AddTimeDuration ( one, two )
      23. 7.5.23 Add24HourDaysToTimeDuration ( d, days )
      24. 7.5.24 AddTimeDurationToEpochNanoseconds ( d, epochNs )
      25. 7.5.25 CompareTimeDuration ( one, two )
      26. 7.5.26 TimeDurationFromEpochNanosecondsDifference ( one, two )
      27. 7.5.27 RoundTimeDurationToIncrement ( d, increment, roundingMode )
      28. 7.5.28 TimeDurationSign ( d )
      29. 7.5.29 DateDurationDays ( dateDuration, plainRelativeTo )
      30. 7.5.30 RoundTimeDuration ( timeDuration, increment, unit, roundingMode )
      31. 7.5.31 TotalTimeDuration ( timeDuration, unit )
      32. 7.5.32 Duration Nudge Result Records
      33. 7.5.33 ComputeNudgeWindow ( sign, duration, originEpochNs, isoDateTime, timeZone, calendar, increment, unit, additionalShift )
      34. 7.5.34 NudgeToCalendarUnit ( sign, duration, originEpochNs, destEpochNs, isoDateTime, timeZone, calendar, increment, unit, roundingMode )
      35. 7.5.35 NudgeToZonedTime ( sign, duration, isoDateTime, timeZone, calendar, increment, unit, roundingMode )
      36. 7.5.36 NudgeToDayOrTime ( duration, destEpochNs, largestUnit, increment, smallestUnit, roundingMode )
      37. 7.5.37 BubbleRelativeDuration ( sign, duration, nudgedEpochNs, isoDateTime, timeZone, calendar, largestUnit, smallestUnit )
      38. 7.5.38 RoundRelativeDuration ( duration, originEpochNs, destEpochNs, isoDateTime, timeZone, calendar, largestUnit, increment, smallestUnit, roundingMode )
      39. 7.5.39 TotalRelativeDuration ( duration, originEpochNs, destEpochNs, isoDateTime, timeZone, calendar, unit )
      40. 7.5.40 TemporalDurationToString ( duration, precision )
      41. 7.5.41 AddDurations ( operation, duration, other )
9. +8 Temporal.Instant Objects
   1. +8.1 The Temporal.Instant Constructor
      1. 8.1.1 Temporal.Instant ( epochNanoseconds )
   2. +8.2 Properties of the Temporal.Instant Constructor
      1. 8.2.1 Temporal.Instant.prototype
      2. 8.2.2 Temporal.Instant.from ( item )
      3. 8.2.3 Temporal.Instant.fromEpochMilliseconds ( epochMilliseconds )
      4. 8.2.4 Temporal.Instant.fromEpochNanoseconds ( epochNanoseconds )
      5. 8.2.5 Temporal.Instant.compare ( one, two )
   3. +8.3 Properties of the Temporal.Instant Prototype Object
      1. 8.3.1 Temporal.Instant.prototype.constructor
      2. 8.3.2 Temporal.Instant.prototype[ %Symbol.toStringTag% ]
      3. 8.3.3 get Temporal.Instant.prototype.epochMilliseconds
      4. 8.3.4 get Temporal.Instant.prototype.epochNanoseconds
      5. 8.3.5 Temporal.Instant.prototype.add ( temporalDurationLike )
      6. 8.3.6 Temporal.Instant.prototype.subtract ( temporalDurationLike )
      7. 8.3.7 Temporal.Instant.prototype.until ( other [ , options ] )
      8. 8.3.8 Temporal.Instant.prototype.since ( other [ , options ] )
      9. 8.3.9 Temporal.Instant.prototype.round ( roundTo )
      10. 8.3.10 Temporal.Instant.prototype.equals ( other )
      11. 8.3.11 Temporal.Instant.prototype.toString ( [ options ] )
      12. 8.3.12 Temporal.Instant.prototype.toLocaleString ( [ locales [ , options ] ] )
      13. 8.3.13 Temporal.Instant.prototype.toJSON ( )
      14. 8.3.14 Temporal.Instant.prototype.valueOf ( )
      15. 8.3.15 Temporal.Instant.prototype.toZonedDateTimeISO ( timeZone )
   4. +8.4 Properties of Temporal.Instant Instances
      1. 8.4.1 Temporal.Instant range
   5. +8.5 Abstract Operations
      1. 8.5.1 IsValidEpochNanoseconds ( epochNanoseconds )
      2. 8.5.2 CreateTemporalInstant ( epochNanoseconds [ , newTarget ] )
      3. 8.5.3 ToTemporalInstant ( item )
      4. 8.5.4 CompareEpochNanoseconds ( epochNanosecondsOne, epochNanosecondsTwo )
      5. 8.5.5 AddInstant ( epochNanoseconds, timeDuration )
      6. 8.5.6 DifferenceInstant ( ns1, ns2, roundingIncrement, smallestUnit, roundingMode )
      7. 8.5.7 RoundTemporalInstant ( ns, increment, unit, roundingMode )
      8. 8.5.8 TemporalInstantToString ( instant, timeZone, precision )
      9. 8.5.9 DifferenceTemporalInstant ( operation, instant, other, options )
      10. 8.5.10 AddDurationToInstant ( operation, instant, temporalDurationLike )
10. +9 Temporal.PlainYearMonth Objects
    1. +9.1 The Temporal.PlainYearMonth Constructor
    1. 9.1.1 Temporal.PlainYearMonth ( isoYear, isoMonth [ , calendar [ , referenceISODay ] ] )
       2. +9.2 Properties of the Temporal.PlainYearMonth Constructor
    1. 9.2.1 Temporal.PlainYearMonth.prototype
    1. 9.2.2 Temporal.PlainYearMonth.from ( item [ , options ] )
    1. 9.2.3 Temporal.PlainYearMonth.compare ( one, two )
       3. +9.3 Properties of the Temporal.PlainYearMonth Prototype Object
    1. 9.3.1 Temporal.PlainYearMonth.prototype.constructor
    1. 9.3.2 Temporal.PlainYearMonth.prototype[ %Symbol.toStringTag% ]
    1. 9.3.3 get Temporal.PlainYearMonth.prototype.calendarId
    1. 9.3.4 get Temporal.PlainYearMonth.prototype.era
    1. 9.3.5 get Temporal.PlainYearMonth.prototype.eraYear
    1. 9.3.6 get Temporal.PlainYearMonth.prototype.year
    1. 9.3.7 get Temporal.PlainYearMonth.prototype.month
    1. 9.3.8 get Temporal.PlainYearMonth.prototype.monthCode
    1. 9.3.9 get Temporal.PlainYearMonth.prototype.daysInYear
    1. 9.3.10 get Temporal.PlainYearMonth.prototype.daysInMonth
    1. 9.3.11 get Temporal.PlainYearMonth.prototype.monthsInYear
    1. 9.3.12 get Temporal.PlainYearMonth.prototype.inLeapYear
    1. 9.3.13 Temporal.PlainYearMonth.prototype.with ( temporalYearMonthLike [ , options ] )
    1. 9.3.14 Temporal.PlainYearMonth.prototype.add ( temporalDurationLike [ , options ] )
    1. 9.3.15 Temporal.PlainYearMonth.prototype.subtract ( temporalDurationLike [ , options ] )
    1. 9.3.16 Temporal.PlainYearMonth.prototype.until ( other [ , options ] )
    1. 9.3.17 Temporal.PlainYearMonth.prototype.since ( other [ , options ] )
    1. 9.3.18 Temporal.PlainYearMonth.prototype.equals ( other )
    1. 9.3.19 Temporal.PlainYearMonth.prototype.toString ( [ options ] )
    1. 9.3.20 Temporal.PlainYearMonth.prototype.toLocaleString ( [ locales [ , options ] ] )
    1. 9.3.21 Temporal.PlainYearMonth.prototype.toJSON ( )
    1. 9.3.22 Temporal.PlainYearMonth.prototype.valueOf ( )
    1. 9.3.23 Temporal.PlainYearMonth.prototype.toPlainDate ( item )
       4. 9.4 Properties of Temporal.PlainYearMonth Instances
       5. +9.5 Abstract Operations
    1. 9.5.1 ISO Year-Month Records
    1. 9.5.2 ToTemporalYearMonth ( item [ , options ] )
    1. 9.5.3 ISOYearMonthWithinLimits ( isoDate )
    1. 9.5.4 BalanceISOYearMonth ( year, month )
    1. 9.5.5 CreateTemporalYearMonth ( isoDate, calendar [ , newTarget ] )
    1. 9.5.6 TemporalYearMonthToString ( yearMonth, showCalendar )
    1. 9.5.7 DifferenceTemporalPlainYearMonth ( operation, yearMonth, other, options )
    1. 9.5.8 AddDurationToYearMonth ( operation, yearMonth, temporalDurationLike, options )
11. +10 Temporal.PlainMonthDay Objects
    1. +10.1 The Temporal.PlainMonthDay Constructor
    1. 10.1.1 Temporal.PlainMonthDay ( isoMonth, isoDay [ , calendar [ , referenceISOYear ] ] )
       2. +10.2 Properties of the Temporal.PlainMonthDay Constructor
    1. 10.2.1 Temporal.PlainMonthDay.prototype
    1. 10.2.2 Temporal.PlainMonthDay.from ( item [ , options ] )
       3. +10.3 Properties of the Temporal.PlainMonthDay Prototype Object
    1. 10.3.1 Temporal.PlainMonthDay.prototype.constructor
    1. 10.3.2 Temporal.PlainMonthDay.prototype[ %Symbol.toStringTag% ]
    1. 10.3.3 get Temporal.PlainMonthDay.prototype.calendarId
    1. 10.3.4 get Temporal.PlainMonthDay.prototype.monthCode
    1. 10.3.5 get Temporal.PlainMonthDay.prototype.day
    1. 10.3.6 Temporal.PlainMonthDay.prototype.with ( temporalMonthDayLike [ , options ] )
    1. 10.3.7 Temporal.PlainMonthDay.prototype.equals ( other )
    1. 10.3.8 Temporal.PlainMonthDay.prototype.toString ( [ options ] )
    1. 10.3.9 Temporal.PlainMonthDay.prototype.toLocaleString ( [ locales [ , options ] ] )
    1. 10.3.10 Temporal.PlainMonthDay.prototype.toJSON ( )
    1. 10.3.11 Temporal.PlainMonthDay.prototype.valueOf ( )
    1. 10.3.12 Temporal.PlainMonthDay.prototype.toPlainDate ( item )
       4. 10.4 Properties of Temporal.PlainMonthDay Instances
       5. +10.5 Abstract Operations
    1. 10.5.1 ToTemporalMonthDay ( item [ , options ] )
    1. 10.5.2 CreateTemporalMonthDay ( isoDate, calendar [ , newTarget ] )
    1. 10.5.3 TemporalMonthDayToString ( monthDay, showCalendar )
12. +11 Time Zones
    1. +11.1 Abstract Operations
    1. 11.1.1 GetAvailableNamedTimeZoneIdentifier ( timeZoneIdentifier )
    1. 11.1.2 GetISOPartsFromEpoch ( epochNanoseconds )
    1. 11.1.3 GetNamedTimeZoneNextTransition ( timeZoneIdentifier, epochNanoseconds )
    1. 11.1.4 GetNamedTimeZonePreviousTransition ( timeZoneIdentifier, epochNanoseconds )
    1. 11.1.5 FormatOffsetTimeZoneIdentifier ( offsetMinutes [ , style ] )
    1. 11.1.6 FormatUTCOffsetNanoseconds ( offsetNanoseconds )
    1. 11.1.7 FormatDateTimeUTCOffsetRounded ( offsetNanoseconds )
    1. 11.1.8 ToTemporalTimeZoneIdentifier ( temporalTimeZoneLike )
    1. 11.1.9 GetOffsetNanosecondsFor ( timeZone, epochNs )
    1. 11.1.10 GetISODateTimeFor ( timeZone, epochNs )
    1. 11.1.11 GetEpochNanosecondsFor ( timeZone, isoDateTime, disambiguation )
    1. 11.1.12 DisambiguatePossibleEpochNanoseconds ( possibleEpochNs, timeZone, isoDateTime, disambiguation )
    1. 11.1.13 GetPossibleEpochNanoseconds ( timeZone, isoDateTime )
    1. 11.1.14 GetStartOfDay ( timeZone, isoDate )
    1. 11.1.15 TimeZoneEquals ( one, two )
    1. 11.1.16 ParseTimeZoneIdentifier ( identifier )
13. +12 Calendars
    1. +12.1 Calendar Types
    1. 12.1.1 CanonicalizeCalendar ( id )
    1. 12.1.2 AvailableCalendars ( )
       2. +12.2 Month Codes
    1. 12.2.1 ParseMonthCode ( argument )
    1. 12.2.2 CreateMonthCode ( monthNumber, isLeapMonth )
       3. +12.3 Abstract Operations
    1. 12.3.1 Calendar Date Records
    1. 12.3.2 Calendar Fields Records
    1. 12.3.3 PrepareCalendarFields ( calendar, fields, calendarFieldNames, nonCalendarFieldNames, requiredFieldNames )
    1. 12.3.4 CalendarFieldKeysPresent ( fields )
    1. 12.3.5 CalendarMergeFields ( calendar, fields, additionalFields )
    1. 12.3.6 NonISODateAdd ( calendar, isoDate, duration, overflow )
    1. 12.3.7 CalendarDateAdd ( calendar, isoDate, duration, overflow )
    1. 12.3.8 NonISODateUntil ( calendar, one, two, largestUnit )
    1. 12.3.9 CalendarDateUntil ( calendar, one, two, largestUnit )
    1. 12.3.10 ToTemporalCalendarIdentifier ( temporalCalendarLike )
    1. 12.3.11 GetTemporalCalendarIdentifierWithISODefault ( item )
    1. 12.3.12 CalendarDateFromFields ( calendar, fields, overflow )
    1. 12.3.13 CalendarYearMonthFromFields ( calendar, fields, overflow )
    1. 12.3.14 CalendarMonthDayFromFields ( calendar, fields, overflow )
    1. 12.3.15 FormatCalendarAnnotation ( id, showCalendar )
    1. 12.3.16 CalendarEquals ( one, two )
    1. 12.3.17 ISODaysInMonth ( year, month )
    1. 12.3.18 ISOWeekOfYear ( isoDate )
    1. 12.3.19 ISODayOfYear ( isoDate )
    1. 12.3.20 ISODayOfWeek ( isoDate )
    1. 12.3.21 NonISOCalendarDateToISO ( calendar, fields, overflow )
    1. 12.3.22 CalendarDateToISO ( calendar, fields, overflow )
    1. 12.3.23 NonISOMonthDayToISOReferenceDate ( calendar, fields, overflow )
    1. 12.3.24 CalendarMonthDayToISOReferenceDate ( calendar, fields, overflow )
    1. 12.3.25 NonISOCalendarISOToDate ( calendar, isoDate )
    1. 12.3.26 CalendarISOToDate ( calendar, isoDate )
    1. 12.3.27 CalendarExtraFields ( calendar, fields )
    1. 12.3.28 NonISOFieldKeysToIgnore ( calendar, keys )
    1. 12.3.29 CalendarFieldKeysToIgnore ( calendar, keys )
    1. 12.3.30 NonISOResolveFields ( calendar, fields, type )
    1. 12.3.31 CalendarResolveFields ( calendar, fields, type )
14. +13 Abstract Operations
    1. 13.1 ISODateToEpochDays ( year, month, date )
    2. 13.2 EpochDaysToEpochMs ( day, time )
    3. 13.3 Date Equations
    4. 13.4 CheckISODaysRange ( isoDate )
    5. 13.5 Units
    6. 13.6 GetTemporalOverflowOption ( options )
    7. 13.7 GetTemporalDisambiguationOption ( options )
    8. 13.8 NegateRoundingMode ( roundingMode )
    9. 13.9 GetTemporalOffsetOption ( options, fallback )
    10. 13.10 GetTemporalShowCalendarNameOption ( options )
    11. 13.11 GetTemporalShowTimeZoneNameOption ( options )
    12. 13.12 GetTemporalShowOffsetOption ( options )
    13. 13.13 GetDirectionOption ( options )
    14. 13.14 ValidateTemporalRoundingIncrement ( increment, dividend, inclusive )
    15. 13.15 GetTemporalFractionalSecondDigitsOption ( options )
    16. 13.16 ToSecondsStringPrecisionRecord ( smallestUnit, fractionalDigitCount )
    17. 13.17 GetTemporalUnitValuedOption ( options, key, default )
    18. 13.18 ValidateTemporalUnitValue ( value, unitGroup [ , extraValues ] )
    19. 13.19 GetTemporalRelativeToOption ( options )
    20. 13.20 LargerOfTwoTemporalUnits ( u1, u2 )
    21. 13.21 IsCalendarUnit ( unit )
    22. 13.22 TemporalUnitCategory ( unit )
    23. 13.23 MaximumTemporalDurationRoundingIncrement ( unit )
    24. 13.24 IsPartialTemporalObject ( value )
    25. 13.25 FormatFractionalSeconds ( subSecondNanoseconds, precision )
    26. 13.26 FormatTimeString ( hour, minute, second, subSecondNanoseconds, precision [ , style ] )
    27. 13.27 GetUnsignedRoundingMode ( roundingMode, sign )
    28. 13.28 ApplyUnsignedRoundingMode ( x, r1, r2, unsignedRoundingMode )
    29. 13.29 RoundNumberToIncrement ( x, increment, roundingMode )
    30. 13.30 RoundNumberToIncrementAsIfPositive ( x, increment, roundingMode )
    31. +13.31 RFC 9557 / ISO 8601 grammar
    32. 13.31.1 SS: IsValidMonthDay
    33. 13.31.2 SS: IsValidDate
    34. 13.31.3 SS: Early Errors
        32. 13.32 ISO String Time Zone Parse Records
        33. 13.33 Time Zone Identifier Parse Records
        34. 13.34 ISO Date-Time Parse Records
        35. 13.35 ParseISODateTime ( isoString, allowedFormats )
        36. 13.36 ParseTemporalCalendarString ( string )
        37. 13.37 ParseTemporalDurationString ( isoString )
        38. 13.38 ParseTemporalTimeZoneString ( timeZoneString )
        39. 13.39 ToPositiveIntegerWithTruncation ( argument )
        40. 13.40 ToIntegerWithTruncation ( argument )
        41. 13.41 ToOffsetString ( argument )
        42. 13.42 ISODateToFields ( calendar, isoDate, type )
        43. 13.43 GetDifferenceSettings ( operation, options, unitGroup, disallowedUnits, fallbackSmallestUnit, smallestLargestDefaultUnit )
15. +14 Amendments to the ECMAScript® 2023 Language Specification
    1. 14.1 The String Type
    2. +14.2 The Object Type
    3. 14.2.1 Well-Known Intrinsic Objects
       3. 14.3 The Year-Week Record Specification Type
       4. 14.4 Mathematical Operations
       5. +14.5 Abstract Operations
    4. +14.5.1 Type Conversion
       1. 14.5.1.1 ToIntegerIfIntegral ( argument )
    5. +14.5.2 Operations for Reading Options
       1. 14.5.2.1 GetOption ( options, property, type, values, default )
       2. 14.5.2.2 GetRoundingModeOption ( options, fallback )
       3. 14.5.2.3 GetRoundingIncrementOption ( options )
          6. +14.6 Overview of Date Objects and Definitions of Abstract Operations
    6. 14.6.1 GetUTCEpochNanoseconds ( ~~year~~ , ~~month~~ , ~~day~~ , ~~hour~~ , ~~minute~~ , ~~second~~ , ~~millisecond~~ , ~~microsecond~~ , ~~nanosecond~~ , isoDateTime )
    7. 14.6.2 Time Zone Identifiers
    8. 14.6.3 GetNamedTimeZoneEpochNanoseconds ( timeZoneIdentifier, ~~year~~ , ~~month~~ , ~~day~~ , ~~hour~~ , ~~minute~~ , ~~second~~ , ~~millisecond~~ , ~~microsecond~~ , ~~nanosecond~~ , isoDateTime )
    9. 14.6.4 SystemTimeZoneIdentifier ( )
    10. 14.6.5 Time Zone Offset String ~~Format~~ Formats
    11. 14.6.6 LocalTime ( t )
    12. 14.6.7 UTC ( t )
    13. 14.6.8 TimeString ( tv )
    14. 14.6.9 TimeZoneString ( tv )
    15. 14.6.10 IsOffsetTimeZoneIdentifier ( offsetString )
    16. 14.6.11 ParseDateTimeUTCOffset ( offsetString )
        7. +14.7 The Date Constructor
    17. 14.7.1 Date ( ...values )
        8. +14.8 Properties of the Date Constructor
    18. 14.8.1 Date.now ( )
        9. +14.9 Properties of the Date Prototype Object
    19. 14.9.1 Date.prototype.toTemporalInstant ( )
16. +15 Amendments to the ECMAScript® 2023 Internationalization API Specification
    1. 15.1 Case Sensitivity and Case Mapping
    2. +15.2 Calendar Types
    3. 15.2.1 AvailableCalendars ( )
       3. +15.3 Abstract Operations
    4. 15.3.1 GetOption ( options, property, type, values, default )
    5. 15.3.2 GetUnsignedRoundingMode ( roundingMode, sign )
    6. 15.3.3 ApplyUnsignedRoundingMode ( x, r1, r2, unsignedRoundingMode )
       4. +15.4 The Intl.DateTimeFormat Constructor
    7. 15.4.1 CreateDateTimeFormat ( newTarget, locales, options, required, defaults [ , toLocaleStringTimeZone ] )
       5. +15.5 Properties of the Intl.DateTimeFormat Constructor
    8. 15.5.1 Internal slots
       6. +15.6 Abstract Operations for DateTimeFormat Objects
    9. 15.6.1 GetDateTimeFormat ( formats, matcher, options, required, defaults, inherit )
    10. 15.6.2 AdjustDateTimeStyleFormat ( formats, baseFormat, matcher, allowedOptions )
    11. 15.6.3 DateTime Format Functions
    12. 15.6.4 FormatDateTimePattern ( dateTimeFormat, format, pattern, epochNanoseconds, isPlain )
    13. 15.6.5 PartitionDateTimePattern ( dateTimeFormat, x )
    14. 15.6.6 FormatDateTime ( dateTimeFormat, x )
    15. 15.6.7 FormatDateTimeToParts ( dateTimeFormat, x )
    16. 15.6.8 PartitionDateTimeRangePattern ( dateTimeFormat, x, y )
    17. 15.6.9 FormatDateTimeRange ( dateTimeFormat, x, y )
    18. 15.6.10 FormatDateTimeRangeToParts ( dateTimeFormat, x, y )
    19. 15.6.11 ToDateTimeFormattable ( value )
    20. 15.6.12 IsTemporalObject ( value )
    21. 15.6.13 SameTemporalType ( x, y )
    22. 15.6.14 Value Format Records
    23. 15.6.15 HandleDateTimeTemporalDate ( dateTimeFormat, temporalDate )
    24. 15.6.16 HandleDateTimeTemporalYearMonth ( dateTimeFormat, temporalYearMonth )
    25. 15.6.17 HandleDateTimeTemporalMonthDay ( dateTimeFormat, temporalMonthDay )
    26. 15.6.18 HandleDateTimeTemporalTime ( dateTimeFormat, temporalTime )
    27. 15.6.19 HandleDateTimeTemporalDateTime ( dateTimeFormat, dateTime )
    28. 15.6.20 HandleDateTimeTemporalInstant ( dateTimeFormat, instant )
    29. 15.6.21 HandleDateTimeOthers ( dateTimeFormat, x )
    30. 15.6.22 HandleDateTimeValue ( dateTimeFormat, x )
    31. 15.6.23 ToLocalTime ( epochNs, calendar, timeZoneIdentifier )
        7. +15.7 Properties of the Intl.DateTimeFormat Prototype Object
    32. 15.7.1 Intl.DateTimeFormat.prototype.formatToParts ( date )
    33. 15.7.2 Intl.DateTimeFormat.prototype.formatRange ( startDate, endDate )
    34. 15.7.3 Intl.DateTimeFormat.prototype.formatRangeToParts ( startDate, endDate )
        8. 15.8 Properties of Intl.DateTimeFormat Instances
        9. +15.9 Abstract Operations for DurationFormat Objects
    35. 15.9.1 Duration Records
    36. 15.9.2 ToIntegerIfIntegral ( argument )
    37. 15.9.3 ToDurationRecord ( input )
    38. 15.9.4 DurationSign ( duration )
    39. 15.9.5 IsValidDuration ( years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds )
    40. 15.9.6 ComputeFractionalDigits ( durationFormat, duration )
    41. 15.9.7 FormatNumericUnits ( durationFormat, duration, firstNumericUnit, signDisplayed )
    42. 15.9.8 PartitionDurationFormatPattern ( durationFormat, duration )
        10. +15.10 Properties of the Intl.DurationFormat Prototype Object
    43. 15.10.1 Intl.DurationFormat.prototype.format ( ~~duration~~ durationLike )
    44. 15.10.2 Intl.DurationFormat.prototype.formatToParts ( ~~duration~~ durationLike )
        11. +15.11 Locale Sensitive Functions of the ECMAScript Language Specification
    45. +15.11.1 Properties of the Temporal.Duration Prototype Object
        1. 15.11.1.1 Temporal.Duration.prototype.toLocaleString ( [ locales [ , options ] ] )
    46. +15.11.2 Properties of the Temporal.Instant Prototype Object
        1. 15.11.2.1 Temporal.Instant.prototype.toLocaleString ( [ locales [ , options ] ] )
    47. +15.11.3 Properties of the Temporal.PlainDate Prototype Object
        1. 15.11.3.1 Temporal.PlainDate.prototype.toLocaleString ( [ locales [ , options ] ] )
    48. +15.11.4 Properties of the Temporal.PlainDateTime Prototype Object
        1. 15.11.4.1 Temporal.PlainDateTime.prototype.toLocaleString ( [ locales [ , options ] ] )
    49. +15.11.5 Properties of the Temporal.PlainMonthDay Prototype Object
        1. 15.11.5.1 Temporal.PlainMonthDay.prototype.toLocaleString ( [ locales [ , options ] ] )
    50. +15.11.6 Properties of the Temporal.PlainTime Prototype Object
        1. 15.11.6.1 Temporal.PlainTime.prototype.toLocaleString ( [ locales [ , options ] ] )
    51. +15.11.7 Properties of the Temporal.PlainYearMonth Prototype Object
        1. 15.11.7.1 Temporal.PlainYearMonth.prototype.toLocaleString ( [ locales [ , options ] ] )
    52. +15.11.8 Properties of the Temporal.ZonedDateTime Prototype Object
        1. 15.11.8.1 Temporal.ZonedDateTime.prototype.toLocaleString ( [ locales [ , options ] ] )
17. Copyright & Software License

# Proposal proposal-temporal

# Stage 4 Draft / May 21, 2026

# Temporal

# Introduction

The venerable ECMAScript Date object has a number of challenges, including lack of immutability, lack of support for time zones, lack of support for use cases that require dates only or times only, a confusing and non-ergonomic API, and many other challenges.

The Temporal set of types addresses these challenges with a built-in date and time API for ECMAScript that includes:

- First-class support for all time zones, including DST-safe arithmetic
- Strongly-typed objects for dates, times, date/time values, year/month values, month/day values, "zoned" date/time values, and durations
- Immutability for all Temporal objects
- String serialization and interoperability via standardized formats
- Compliance with industry standards like RFC 9557, RFC 3339, RFC 5545 (iCalendar), and ISO 8601
- Full support for non-Gregorian calendars

Figure 1: Temporal Object Relationships Figure 2: Temporal String Persistence

This specification consists of three parts:

- The specification of the Temporal object and everything related to it, proposed to be added to ECMA-262 in new sections;
- A list of amendments to be made to ECMA-262, other than the new sections above;
- A list of amendments to be made to ECMA-402.

# 1 The Temporal Object

The Temporal object:

- is the intrinsic object %Temporal%.
- is the initial value of the "Temporal" property of the global object.
- is an ordinary object.
- has a [[Prototype]] internal slot whose value is %Object.prototype%.
- is not a function object.
- does not have a [[Construct]] internal method; it cannot be used as a constructor with the `new` operator.
- does not have a [[Call]] internal method; it cannot be invoked as a function.

# 1.1 Value Properties of the Temporal Object

# 1.1.1 Temporal [ %Symbol.toStringTag% ]

The initial value of the %Symbol.toStringTag% property is the String "Temporal".

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }.

# 1.2 Constructor Properties of the Temporal Object

# 1.2.1 Temporal.Instant ( . . . )

See 8.

# 1.2.2 Temporal.PlainDateTime ( . . . )

See 5.

# 1.2.3 Temporal.PlainDate ( . . . )

See 3.

# 1.2.4 Temporal.PlainTime ( . . . )

See 4.

# 1.2.5 Temporal.PlainYearMonth ( . . . )

See 9.

# 1.2.6 Temporal.PlainMonthDay ( . . . )

See 10.

# 1.2.7 Temporal.Duration ( . . . )

See 7.

# 1.2.8 Temporal.ZonedDateTime ( . . . )

See 6.

# 1.3 Other Properties of the Temporal Object

# 1.3.1 Temporal.Now

See 2.

# 2 The Temporal.Now Object

The Temporal.Now object:

- is an ordinary object.
- has a [[Prototype]] internal slot whose value is %Object.prototype%.
- is not a function object.
- does not have a [[Construct]] internal method; it cannot be used as a constructor with the `new` operator.
- does not have a [[Call]] internal method; it cannot be invoked as a function.

# 2.1 Value Properties of the Temporal.Now Object

# 2.1.1 Temporal.Now [ %Symbol.toStringTag% ]

The initial value of the %Symbol.toStringTag% property is the String "Temporal.Now".

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }.

# 2.2 Function Properties of the Temporal.Now Object

# 2.2.1 Temporal.Now.timeZoneId ( )

This function performs the following steps when called:

1. Return SystemTimeZoneIdentifier().

# 2.2.2 Temporal.Now.instant ( )

This function performs the following steps when called:

1. Let ns be SystemUTCEpochNanoseconds().
2. Return ! CreateTemporalInstant(ns).

# 2.2.3 Temporal.Now.plainDateTimeISO ( [ temporalTimeZoneLike ] )

This function performs the following steps when called:

1. Let isoDateTime be ? SystemDateTime(temporalTimeZoneLike).
2. Return ! CreateTemporalDateTime(isoDateTime, "iso8601").

# 2.2.4 Temporal.Now.zonedDateTimeISO ( [ temporalTimeZoneLike ] )

This function performs the following steps when called:

1. If temporalTimeZoneLike is undefined, then
   1. Let timeZone be SystemTimeZoneIdentifier().
2. Else,
   1. Let timeZone be ? ToTemporalTimeZoneIdentifier(temporalTimeZoneLike).
3. Let ns be SystemUTCEpochNanoseconds().
4. Return ! CreateTemporalZonedDateTime(ns, timeZone, "iso8601").

# 2.2.5 Temporal.Now.plainDateISO ( [ temporalTimeZoneLike ] )

This function performs the following steps when called:

1. Let isoDateTime be ? SystemDateTime(temporalTimeZoneLike).
2. Return ! CreateTemporalDate(isoDateTime.[[ISODate]], "iso8601").

# 2.2.6 Temporal.Now.plainTimeISO ( [ temporalTimeZoneLike ] )

This function performs the following steps when called:

1. Let isoDateTime be ? SystemDateTime(temporalTimeZoneLike).
2. Return ! CreateTemporalTime(isoDateTime.[[Time]]).

# 2.3 Abstract Operations

# 2.3.1 HostSystemUTCEpochNanoseconds ( global )

The host-defined abstract operation HostSystemUTCEpochNanoseconds takes argument global (a global object) and returns an integer. It allows host environments to reduce the precision of the result. In particular, web browsers artificially limit it to prevent abuse of security flaws (e.g., Spectre) and to avoid certain methods of fingerprinting.

An implementation of HostSystemUTCEpochNanoseconds must conform to the following requirements:

- Its result must be between nsMinInstant and nsMaxInstant.

Note

This requirement is necessary if the system clock is set to a time outside the range that Temporal.Instant can represent. This is not expected to affect implementations in practice.

The default implementation of HostSystemUTCEpochNanoseconds performs the following steps when called:

1. Let ns be the approximate current UTC date and time, in nanoseconds since the epoch.
2. Return the result of clamping ns between nsMinInstant and nsMaxInstant.

ECMAScript hosts that are not web browsers must use the default implementation of HostSystemUTCEpochNanoseconds.

# 2.3.2 SystemUTCEpochMilliseconds ( )

The abstract operation SystemUTCEpochMilliseconds takes no arguments and returns a Number. It returns the current UTC date and time as a Number of milliseconds since the epoch. It performs the following steps when called:

1. Let global be GetGlobalObject().
2. Let nowNs be HostSystemUTCEpochNanoseconds(global).
3. Return 𝔽(floor(nowNs / 106)).

# 2.3.3 SystemUTCEpochNanoseconds ( )

The abstract operation SystemUTCEpochNanoseconds takes no arguments and returns a BigInt. It returns the current UTC date and time as a BigInt of nanoseconds since the epoch. It performs the following steps when called:

1. Let global be GetGlobalObject().
2. Let nowNs be HostSystemUTCEpochNanoseconds(global).
3. Return ℤ(nowNs).

# 2.3.4 SystemDateTime ( temporalTimeZoneLike )

The abstract operation SystemDateTime takes argument temporalTimeZoneLike (an ECMAScript language value) and returns either a normal completion containing an ISO Date-Time Record or a throw completion. It returns the current date and time in the time zone specified by temporalTimeZoneLike, or the system time zone if temporalTimeZoneLike is undefined. It performs the following steps when called:

1. If temporalTimeZoneLike is undefined, then
   1. Let timeZone be SystemTimeZoneIdentifier().
2. Else,
   1. Let timeZone be ? ToTemporalTimeZoneIdentifier(temporalTimeZoneLike).
3. Let epochNs be SystemUTCEpochNanoseconds().
4. Return GetISODateTimeFor(timeZone, epochNs).

# 3 Temporal.PlainDate Objects

A Temporal.PlainDate object is an Object that contains integers corresponding to a particular year, month, and day in the ISO8601 calendar, as well as an Object value used to interpret those integers in a particular calendar.

# 3.1 The Temporal.PlainDate Constructor

The Temporal.PlainDate constructor:

- creates and initializes a new Temporal.PlainDate object when called as a constructor.
- is not intended to be called as a function and will throw an exception when called in that manner.
- may be used as the value of an `extends` clause of a class definition. Subclass constructors that intend to inherit the specified Temporal.PlainDate behaviour must include a super call to the %Temporal.PlainDate% constructor to create and initialize subclass instances with the necessary internal slots.

# 3.1.1 Temporal.PlainDate ( isoYear, isoMonth, isoDay [ , calendar ] )

This function performs the following steps when called:

1. If NewTarget is undefined, throw a TypeError exception.
2. Let y be ? ToIntegerWithTruncation(isoYear).
3. Let m be ? ToIntegerWithTruncation(isoMonth).
4. Let d be ? ToIntegerWithTruncation(isoDay).
5. If calendar is undefined, set calendar to "iso8601".
6. If calendar is not a String, throw a TypeError exception.
7. Set calendar to ? CanonicalizeCalendar(calendar).
8. If IsValidISODate(y, m, d) is false, throw a RangeError exception.
9. Let isoDate be CreateISODateRecord(y, m, d).
10. Return ? CreateTemporalDate(isoDate, calendar, NewTarget).

# 3.2 Properties of the Temporal.PlainDate Constructor

The Temporal.PlainDate constructor:

- has a [[Prototype]] internal slot whose value is %Function.prototype%.
- has the following properties:

# 3.2.1 Temporal.PlainDate.prototype

The initial value of `Temporal.PlainDate.prototype` is %Temporal.PlainDate.prototype%.

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: false }.

# 3.2.2 Temporal.PlainDate.from ( item [ , options ] )

This function performs the following steps when called:

1. Return ? ToTemporalDate(item, options).

# 3.2.3 Temporal.PlainDate.compare ( one, two )

This function performs the following steps when called:

1. Set one to ? ToTemporalDate(one).
2. Set two to ? ToTemporalDate(two).
3. Return 𝔽(CompareISODate(one.[[ISODate]], two.[[ISODate]])).

# 3.3 Properties of the Temporal.PlainDate Prototype Object

The Temporal.PlainDate prototype object

- is itself an ordinary object.
- is not a Temporal.PlainDate instance and does not have a [[InitializedTemporalDate]] internal slot.
- has a [[Prototype]] internal slot whose value is %Object.prototype%.

Note

An ECMAScript implementation that includes the ECMA-402 Internationalization API extends this prototype with additional properties in order to represent calendar data.

# 3.3.1 Temporal.PlainDate.prototype.constructor

The initial value of `Temporal.PlainDate.prototype.constructor` is %Temporal.PlainDate%.

# 3.3.2 Temporal.PlainDate.prototype[ %Symbol.toStringTag% ]

The initial value of the %Symbol.toStringTag% property is the String "Temporal.PlainDate".

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }.

# 3.3.3 get Temporal.PlainDate.prototype.calendarId

`Temporal.PlainDate.prototype.calendarId` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return plainDate.[[Calendar]].

# 3.3.4 get Temporal.PlainDate.prototype.era

`Temporal.PlainDate.prototype.era` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[Era]].

# 3.3.5 get Temporal.PlainDate.prototype.eraYear

`Temporal.PlainDate.prototype.eraYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Let result be CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[EraYear]].
4. If result is undefined, return undefined.
5. Return 𝔽(result).

# 3.3.6 get Temporal.PlainDate.prototype.year

`Temporal.PlainDate.prototype.year` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return 𝔽(CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[Year]]).

# 3.3.7 get Temporal.PlainDate.prototype.month

`Temporal.PlainDate.prototype.month` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return 𝔽(CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[Month]]).

# 3.3.8 get Temporal.PlainDate.prototype.monthCode

`Temporal.PlainDate.prototype.monthCode` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[MonthCode]].

# 3.3.9 get Temporal.PlainDate.prototype.day

`Temporal.PlainDate.prototype.day` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return 𝔽(CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[Day]]).

# 3.3.10 get Temporal.PlainDate.prototype.dayOfWeek

`Temporal.PlainDate.prototype.dayOfWeek` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return 𝔽(CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[DayOfWeek]]).

# 3.3.11 get Temporal.PlainDate.prototype.dayOfYear

`Temporal.PlainDate.prototype.dayOfYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return 𝔽(CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[DayOfYear]]).

# 3.3.12 get Temporal.PlainDate.prototype.weekOfYear

`Temporal.PlainDate.prototype.weekOfYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Let result be CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[WeekOfYear]].[[Week]].
4. If result is undefined, return undefined.
5. Return 𝔽(result).

# 3.3.13 get Temporal.PlainDate.prototype.yearOfWeek

`Temporal.PlainDate.prototype.yearOfWeek` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Let result be CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[WeekOfYear]].[[Year]].
4. If result is undefined, return undefined.
5. Return 𝔽(result).

# 3.3.14 get Temporal.PlainDate.prototype.daysInWeek

`Temporal.PlainDate.prototype.daysInWeek` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return 𝔽(CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[DaysInWeek]]).

# 3.3.15 get Temporal.PlainDate.prototype.daysInMonth

`Temporal.PlainDate.prototype.daysInMonth` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return 𝔽(CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[DaysInMonth]]).

# 3.3.16 get Temporal.PlainDate.prototype.daysInYear

`Temporal.PlainDate.prototype.daysInYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return 𝔽(CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[DaysInYear]]).

# 3.3.17 get Temporal.PlainDate.prototype.monthsInYear

`Temporal.PlainDate.prototype.monthsInYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return 𝔽(CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[MonthsInYear]]).

# 3.3.18 get Temporal.PlainDate.prototype.inLeapYear

`Temporal.PlainDate.prototype.inLeapYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return CalendarISOToDate(plainDate.[[Calendar]], plainDate.[[ISODate]]).[[InLeapYear]].

# 3.3.19 Temporal.PlainDate.prototype.toPlainYearMonth ( )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Let calendar be plainDate.[[Calendar]].
4. Let fields be ISODateToFields(calendar, plainDate.[[ISODate]], date).
5. Let isoDate be ? CalendarYearMonthFromFields(calendar, fields, constrain).
6. Return ! CreateTemporalYearMonth(isoDate, calendar).
7. NOTE: The call to CalendarYearMonthFromFields is necessary in order to create a PlainYearMonth object with the [[Day]] field of the [[ISODate]] internal slot set correctly.

# 3.3.20 Temporal.PlainDate.prototype.toPlainMonthDay ( )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Let calendar be plainDate.[[Calendar]].
4. Let fields be ISODateToFields(calendar, plainDate.[[ISODate]], date).
5. Let isoDate be ? CalendarMonthDayFromFields(calendar, fields, constrain).
6. Return ! CreateTemporalMonthDay(isoDate, calendar).
7. NOTE: The call to CalendarMonthDayFromFields is necessary in order to create a PlainMonthDay object with the [[Year]] field of the [[ISODate]] internal slot set correctly.

# 3.3.21 Temporal.PlainDate.prototype.add ( temporalDurationLike [ , options ] )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return ? AddDurationToDate(add, plainDate, temporalDurationLike, options).

# 3.3.22 Temporal.PlainDate.prototype.subtract ( temporalDurationLike [ , options ] )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return ? AddDurationToDate(subtract, plainDate, temporalDurationLike, options).

# 3.3.23 Temporal.PlainDate.prototype.with ( temporalDateLike [ , options ] )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. If ? IsPartialTemporalObject(temporalDateLike) is false, throw a TypeError exception.
4. Let calendar be plainDate.[[Calendar]].
5. Let fields be ISODateToFields(calendar, plainDate.[[ISODate]], date).
6. Let partialDate be ? PrepareCalendarFields(calendar, temporalDateLike, « year, month, month-code, day », « », partial).
7. Set fields to CalendarMergeFields(calendar, fields, partialDate).
8. Let resolvedOptions be ? GetOptionsObject(options).
9. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
10. Let isoDate be ? CalendarDateFromFields(calendar, fields, overflow).
11. Return ! CreateTemporalDate(isoDate, calendar).

# 3.3.24 Temporal.PlainDate.prototype.withCalendar ( calendarLike )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Let calendar be ? ToTemporalCalendarIdentifier(calendarLike).
4. Return ! CreateTemporalDate(plainDate.[[ISODate]], calendar).

# 3.3.25 Temporal.PlainDate.prototype.until ( other [ , options ] )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return ? DifferenceTemporalPlainDate(until, plainDate, other, options).

# 3.3.26 Temporal.PlainDate.prototype.since ( other [ , options ] )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return ? DifferenceTemporalPlainDate(since, plainDate, other, options).

# 3.3.27 Temporal.PlainDate.prototype.equals ( other )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Set other to ? ToTemporalDate(other).
4. If CompareISODate(plainDate.[[ISODate]], other.[[ISODate]]) ≠ 0, return false.
5. Return CalendarEquals(plainDate.[[Calendar]], other.[[Calendar]]).

# 3.3.28 Temporal.PlainDate.prototype.toPlainDateTime ( [ temporalTime ] )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Let time be ? ToTimeRecordOrMidnight(temporalTime).
4. Let isoDateTime be CombineISODateAndTimeRecord(plainDate.[[ISODate]], time).
5. Return ? CreateTemporalDateTime(isoDateTime, plainDate.[[Calendar]]).

# 3.3.29 Temporal.PlainDate.prototype.toZonedDateTime ( item )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. If item is an Object, then
   1. Let timeZoneLike be ? Get(item, "timeZone").
   2. If timeZoneLike is undefined, then
      1. Let timeZone be ? ToTemporalTimeZoneIdentifier(item).
      2. Let temporalTime be undefined.
   3. Else,
      1. Let timeZone be ? ToTemporalTimeZoneIdentifier(timeZoneLike).
      2. Let temporalTime be ? Get(item, "plainTime").
4. Else,
   1. Let timeZone be ? ToTemporalTimeZoneIdentifier(item).
   2. Let temporalTime be undefined.
5. If temporalTime is undefined, then
   1. Let epochNs be ? GetStartOfDay(timeZone, plainDate.[[ISODate]]).
6. Else,
   1. Set temporalTime to ? ToTemporalTime(temporalTime).
   2. Let isoDateTime be CombineISODateAndTimeRecord(plainDate.[[ISODate]], temporalTime.[[Time]]).
   3. If ISODateTimeWithinLimits(isoDateTime) is false, throw a RangeError exception.
   4. Let epochNs be ? GetEpochNanosecondsFor(timeZone, isoDateTime, compatible).
7. Return ! CreateTemporalZonedDateTime(epochNs, timeZone, plainDate.[[Calendar]]).

# 3.3.30 Temporal.PlainDate.prototype.toString ( [ options ] )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. Let showCalendar be ? GetTemporalShowCalendarNameOption(resolvedOptions).
5. Return TemporalDateToString(plainDate, showCalendar).

# 3.3.31 Temporal.PlainDate.prototype.toLocaleString ( [ locales [ , options ] ] )

An ECMAScript implementation that includes the ECMA-402 Internationalization API must implement this method as specified in the ECMA-402 specification. If an ECMAScript implementation does not include the ECMA-402 API the following specification of this method is used.

The meanings of the optional parameters to this method are defined in the ECMA-402 specification; implementations that do not include ECMA-402 support must not use those parameter positions for anything else.

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return TemporalDateToString(plainDate, auto).

# 3.3.32 Temporal.PlainDate.prototype.toJSON ( )

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Return TemporalDateToString(plainDate, auto).

# 3.3.33 Temporal.PlainDate.prototype.valueOf ( )

This method performs the following steps when called:

1. Throw a TypeError exception.

Note

This method always throws, because in the absence of `valueOf()`, expressions with arithmetic operators such as `plainDate1 > plainDate2` would fall back to being equivalent to `plainDate1.toString() > plainDate2.toString()`. Lexicographical comparison of serialized strings might not seem obviously wrong, because the result would sometimes be correct. Implementations are encouraged to phrase the error message to point users to `Temporal.PlainDate.compare()`, `Temporal.PlainDate.prototype.equals()`, and/or `Temporal.PlainDate.prototype.toString()`.

# 3.4 Properties of Temporal.PlainDate Instances

Temporal.PlainDate instances are ordinary objects that inherit properties from the %Temporal.PlainDate.prototype% intrinsic object. Temporal.PlainDate instances are initially created with the internal slots described in Table 1.

| Table 1: Internal Slots of Temporal.PlainDate Instances Internal Slot | Description                                                                                                |
| --------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| [[InitializedTemporalDate]]                                           | The only specified use of this slot is for distinguishing Temporal.PlainDate instances from other objects. |
| [[ISODate]]                                                           | An ISO Date Record.                                                                                        |
| [[Calendar]]                                                          | A calendar type.                                                                                           |

# 3.5 Abstract Operations for Temporal.PlainDate Objects

# 3.5.1 ISO Date Records

An ISO Date Record is a Record value used to represent a valid calendar date in the ISO 8601 calendar, although the year may be outside of the allowed range for Temporal. ISO Date Records are produced by the abstract operation CreateISODateRecord. For any ISO Date Record d, IsValidISODate(d.[[Year]], d.[[Month]], d.[[Day]]) must return true.

ISO Date Records have the fields listed in Table 2.

| Table 2: ISO Date Record Fields Field Name | Value                                  | Meaning                                                      |
| ------------------------------------------ | -------------------------------------- | ------------------------------------------------------------ |
| [[Year]]                                   | an integer                             | The year in the ISO 8601 calendar.                           |
| [[Month]]                                  | an integer between 1 and 12, inclusive | The number of the month in the ISO 8601 calendar.            |
| [[Day]]                                    | an integer between 1 and 31, inclusive | The number of the day of the month in the ISO 8601 calendar. |

# 3.5.2 CreateISODateRecord ( year, month, day )

The abstract operation CreateISODateRecord takes arguments year (an integer), month (an integer between 1 and 12 inclusive), and day (an integer between 1 and 31 inclusive) and returns an ISO Date Record. It creates an ISO Date Record from the given year, month, and day values. It performs the following steps when called:

1. Assert: IsValidISODate(year, month, day) is true.
2. Return ISO Date Record { [[Year]]: year, [[Month]]: month, [[Day]]: day }.

# 3.5.3 CreateTemporalDate ( isoDate, calendar [ , newTarget ] )

The abstract operation CreateTemporalDate takes arguments isoDate (an ISO Date Record) and calendar (a calendar type) and optional argument newTarget (a constructor) and returns either a normal completion containing a Temporal.PlainDate or a throw completion. It creates a Temporal.PlainDate instance and fills the internal slots with valid values. It performs the following steps when called:

1. If ISODateWithinLimits(isoDate) is false, throw a RangeError exception.
2. If newTarget is not present, set newTarget to %Temporal.PlainDate%.
3. Let object be ? OrdinaryCreateFromConstructor(newTarget, "%Temporal.PlainDate.prototype%", « [[InitializedTemporalDate]], [[ISODate]], [[Calendar]] »).
4. Set object.[[ISODate]] to isoDate.
5. Set object.[[Calendar]] to calendar.
6. Return object.

# 3.5.4 ToTemporalDate ( item [ , options ] )

The abstract operation ToTemporalDate takes argument item (an ECMAScript language value) and optional argument options (an ECMAScript language value) and returns either a normal completion containing a Temporal.PlainDate or a throw completion. Converts item to a new Temporal.PlainDate instance if possible, and throws otherwise. It performs the following steps when called:

1. If options is not present, set options to undefined.
2. If item is an Object, then
   1. If item has an [[InitializedTemporalDate]] internal slot, then
      1. Let resolvedOptions be ? GetOptionsObject(options).
      2. Perform ? GetTemporalOverflowOption(resolvedOptions).
      3. Return ! CreateTemporalDate(item.[[ISODate]], item.[[Calendar]]).
   2. If item has an [[InitializedTemporalZonedDateTime]] internal slot, then
      1. Let isoDateTime be GetISODateTimeFor(item.[[TimeZone]], item.[[EpochNanoseconds]]).
      2. Let resolvedOptions be ? GetOptionsObject(options).
      3. Perform ? GetTemporalOverflowOption(resolvedOptions).
      4. Return ! CreateTemporalDate(isoDateTime.[[ISODate]], item.[[Calendar]]).
   3. If item has an [[InitializedTemporalDateTime]] internal slot, then
      1. Let resolvedOptions be ? GetOptionsObject(options).
      2. Perform ? GetTemporalOverflowOption(resolvedOptions).
      3. Return ! CreateTemporalDate(item.[[ISODateTime]].[[ISODate]], item.[[Calendar]]).
   4. Let calendar be ? GetTemporalCalendarIdentifierWithISODefault(item).
   5. Let fields be ? PrepareCalendarFields(calendar, item, « year, month, month-code, day », «», «»).
   6. Let resolvedOptions be ? GetOptionsObject(options).
   7. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
   8. Let isoDate be ? CalendarDateFromFields(calendar, fields, overflow).
   9. Return ! CreateTemporalDate(isoDate, calendar).
3. If item is not a String, throw a TypeError exception.
4. Let result be ? ParseISODateTime(item, « TemporalDateTimeString[~Zoned] »).
5. Let calendar be result.[[Calendar]].
6. If calendar is empty, set calendar to "iso8601".
7. Set calendar to ? CanonicalizeCalendar(calendar).
8. Let resolvedOptions be ? GetOptionsObject(options).
9. Perform ? GetTemporalOverflowOption(resolvedOptions).
10. Let isoDate be CreateISODateRecord(result.[[Year]], result.[[Month]], result.[[Day]]).
11. Return ? CreateTemporalDate(isoDate, calendar).

# 3.5.5 CompareSurpasses ( sign, year, monthOrCode, day, target )

The abstract operation CompareSurpasses takes arguments sign (either -1 or 1), year (an integer), monthOrCode (either an integer or a month code), day (an integer), and target (a Calendar Date Record) and returns a Boolean. The return value indicates whether the calendar date formed by year, monthOrCode, and day, which need not exist, surpasses target in the direction denoted by sign. It performs the following steps when called:

1. If year ≠ target.[[Year]], then
   1. If sign × (year \- target.[[Year]]) > 0, return true.
2. Else if monthOrCode is a month code and monthOrCode is not target.[[MonthCode]], then
   1. If sign > 0, then
      1. If monthOrCode is lexicographically greater than target.[[MonthCode]], return true.
   2. Else,
      1. If target.[[MonthCode]] is lexicographically greater than monthOrCode, return true.
3. Else if monthOrCode is an integer and monthOrCode ≠ target.[[Month]], then
   1. If sign × (monthOrCode \- target.[[Month]]) > 0, return true.
4. Else if day ≠ target.[[Day]], then
   1. If sign × (day \- target.[[Day]]) > 0, return true.
5. Return false.

# 3.5.6 ISODateSurpasses ( sign, baseDate, isoDate2, years, months, weeks, days )

The abstract operation ISODateSurpasses takes arguments sign (either -1 or 1), baseDate (an ISO Date Record), isoDate2 (an ISO Date Record), years (an integer), months (an integer), weeks (an integer), and days (an integer) and returns a Boolean. The return value indicates whether the date isoDate1, the result of adding the duration denoted by years, months, weeks, and days to baseDate, surpasses isoDate2 in the direction denoted by sign. If weeks and days are both zero, then isoDate1 need not exist (for example, it could be February 30). Note that this operation is specific to date difference calculations and is not the same as CompareISODate. It performs the following steps when called:

1. Let parts be CalendarISOToDate("iso8601", baseDate).
2. Let target be CalendarISOToDate("iso8601", isoDate2).
3. Let y0 be parts.[[Year]] \+ years.
4. If CompareSurpasses(sign, y0, parts.[[MonthCode]], parts.[[Day]], target) is true, return true.
5. If months = 0, return false.
6. Let m0 be parts.[[Month]] \+ months.
7. Let monthsAdded be BalanceISOYearMonth(y0, m0).
8. If CompareSurpasses(sign, monthsAdded.[[Year]], monthsAdded.[[Month]], parts.[[Day]], target) is true, return true.
9. If weeks = 0 and days = 0, return false.
10. Let regulatedDate be ! RegulateISODate(monthsAdded.[[Year]], monthsAdded.[[Month]], parts.[[Day]], constrain).
11. Let daysInWeek be 7.
12. Let balancedDate be AddDaysToISODate(regulatedDate, daysInWeek * weeks \+ days).
13. Return CompareSurpasses(sign, balancedDate.[[Year]], balancedDate.[[Month]], balancedDate.[[Day]], target).

Note

This operation intentionally uses overflow constrain when regulating the year-month. As a result, adding the duration returned by `.since` or `.until` to baseDate may cause a RangeError exception when overflow is reject.

# 3.5.7 RegulateISODate ( year, month, day, overflow )

The abstract operation RegulateISODate takes arguments year (an integer), month (an integer), day (an integer), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date Record or a throw completion. It performs the overflow correction specified by overflow on the values year, month, and day, in order to arrive at a valid date in the ISO 8601 calendar, as determined by IsValidISODate. For reject, values that do not form a valid date cause an exception to be thrown. For constrain, values that do not form a valid date are independently clamped to their respective valid range in order of descending unit magnitude. It performs the following steps when called:

1. If overflow is constrain, then
   1. Set month to the result of clamping month between 1 and 12.
   2. Let daysInMonth be ISODaysInMonth(year, month).
   3. Set day to the result of clamping day between 1 and daysInMonth.
2. Else,
   1. Assert: overflow is reject.
   2. If IsValidISODate(year, month, day) is false, throw a RangeError exception.
3. Return CreateISODateRecord(year, month, day).

# 3.5.8 IsValidISODate ( year, month, day )

The abstract operation IsValidISODate takes arguments year (an integer), month (an integer), and day (an integer) and returns a Boolean. The return value is true if its arguments form a valid date in the ISO 8601 calendar, and false otherwise. This includes dates that may fall outside of the allowed range for Temporal. It performs the following steps when called:

1. If month < 1 or month > 12, return false.
2. Let daysInMonth be ISODaysInMonth(year, month).
3. If day < 1 or day > daysInMonth, return false; else return true.

# 3.5.9 AddDaysToISODate ( isoDate, days )

The abstract operation AddDaysToISODate takes arguments isoDate (an ISO Date Record) and days (an integer) and returns an ISO Date Record. It adds days to isoDate into a valid calendar date in the ISO 8601 calendar as given by IsValidISODate, by overflowing out-of-range month or day values into the next-highest unit. This date may be outside the range given by ISODateTimeWithinLimits. It performs the following steps when called:

1. Let epochDays be ISODateToEpochDays(isoDate.[[Year]], isoDate.[[Month]] \- 1, isoDate.[[Day]]) + days.
2. Let ms be EpochDaysToEpochMs(epochDays, 0).
3. Return CreateISODateRecord(EpochTimeToEpochYear(ms), EpochTimeToMonthInYear(ms) + 1, EpochTimeToDate(ms)).

# 3.5.10 PadISOYear ( y )

The abstract operation PadISOYear takes argument y (an integer) and returns a String. It returns a String representation of y suitable for inclusion in an ISO 8601 string, either in 4-digit format or 6-digit format with sign. It performs the following steps when called:

1. If y ≥ 0 and y ≤ 9999, return ToZeroPaddedDecimalString(y, 4).
2. If y > 0, let yearSign be "+"; else let yearSign be "-".
3. Let year be ToZeroPaddedDecimalString(abs(y), 6).
4. Return the string-concatenation of yearSign and year.

# 3.5.11 TemporalDateToString ( temporalDate, showCalendar )

The abstract operation TemporalDateToString takes arguments temporalDate (a Temporal.PlainDate) and showCalendar (one of auto, always, never, or critical) and returns a String. It formats temporalDate to an ISO 8601 / RFC 9557 string. It performs the following steps when called:

1. Let year be PadISOYear(temporalDate.[[ISODate]].[[Year]]).
2. Let month be ToZeroPaddedDecimalString(temporalDate.[[ISODate]].[[Month]], 2).
3. Let day be ToZeroPaddedDecimalString(temporalDate.[[ISODate]].[[Day]], 2).
4. Let calendar be FormatCalendarAnnotation(temporalDate.[[Calendar]], showCalendar).
5. Return the string-concatenation of year, the code unit 0x002D (HYPHEN-MINUS), month, the code unit 0x002D (HYPHEN-MINUS), day, and calendar.

# 3.5.12 ISODateWithinLimits ( isoDate )

The abstract operation ISODateWithinLimits takes argument isoDate (an ISO Date Record) and returns a Boolean. The return value is true if the date in the ISO 8601 calendar given by the argument is within the representable range of `Temporal.PlainDate`, and false otherwise.

Note

Deferring to ISODateTimeWithinLimits with an hour of 12 avoids trouble at the extremes of the representable range of Temporal.PlainDateTime, which stops just before midnight on each end.

It performs the following steps when called:

1. Let isoDateTime be CombineISODateAndTimeRecord(isoDate, NoonTimeRecord()).
2. Return ISODateTimeWithinLimits(isoDateTime).

# 3.5.13 CompareISODate ( isoDate1, isoDate2 )

The abstract operation CompareISODate takes arguments isoDate1 (an ISO Date Record) and isoDate2 (an ISO Date Record) and returns one of -1, 0, or 1. It performs a comparison of the two dates denoted by isoDate1 and isoDate2 according to ISO 8601 calendar arithmetic. It performs the following steps when called:

1. If isoDate1.[[Year]] > isoDate2.[[Year]], return 1.
2. If isoDate1.[[Year]] < isoDate2.[[Year]], return -1.
3. If isoDate1.[[Month]] > isoDate2.[[Month]], return 1.
4. If isoDate1.[[Month]] < isoDate2.[[Month]], return -1.
5. If isoDate1.[[Day]] > isoDate2.[[Day]], return 1.
6. If isoDate1.[[Day]] < isoDate2.[[Day]], return -1.
7. Return 0.

# 3.5.14 DifferenceTemporalPlainDate ( operation, temporalDate, other, options )

The abstract operation DifferenceTemporalPlainDate takes arguments operation (either since or until), temporalDate (a Temporal.PlainDate), other (an ECMAScript language value), and options (an ECMAScript language value) and returns either a normal completion containing a Temporal.Duration or a throw completion. It computes the difference between the two times represented by temporalDate and other, optionally rounds it, and returns it as a Temporal.Duration object. It performs the following steps when called:

1. Set other to ? ToTemporalDate(other).
2. If CalendarEquals(temporalDate.[[Calendar]], other.[[Calendar]]) is false, throw a RangeError exception.
3. Let resolvedOptions be ? GetOptionsObject(options).
4. Let settings be ? GetDifferenceSettings(operation, resolvedOptions, date, « », day, day).
5. If CompareISODate(temporalDate.[[ISODate]], other.[[ISODate]]) = 0, then
   1. Return ! CreateTemporalDuration(0, 0, 0, 0, 0, 0, 0, 0, 0, 0).
6. Let dateDifference be CalendarDateUntil(temporalDate.[[Calendar]], temporalDate.[[ISODate]], other.[[ISODate]], settings.[[LargestUnit]]).
7. Let duration be CombineDateAndTimeDuration(dateDifference, 0).
8. If settings.[[SmallestUnit]] is not day or settings.[[RoundingIncrement]] ≠ 1, then
   1. Let isoDateTime be CombineISODateAndTimeRecord(temporalDate.[[ISODate]], MidnightTimeRecord()).
   2. Let originEpochNs be GetUTCEpochNanoseconds(isoDateTime).
   3. Let isoDateTimeOther be CombineISODateAndTimeRecord(other.[[ISODate]], MidnightTimeRecord()).
   4. Let destEpochNs be GetUTCEpochNanoseconds(isoDateTimeOther).
   5. Set duration to ? RoundRelativeDuration(duration, originEpochNs, destEpochNs, isoDateTime, unset, temporalDate.[[Calendar]], settings.[[LargestUnit]], settings.[[RoundingIncrement]], settings.[[SmallestUnit]], settings.[[RoundingMode]]).
9. Let result be ! TemporalDurationFromInternal(duration, day).
10. If operation is since, set result to CreateNegatedTemporalDuration(result).
11. Return result.

# 3.5.15 AddDurationToDate ( operation, temporalDate, temporalDurationLike, options )

The abstract operation AddDurationToDate takes arguments operation (either add or subtract), temporalDate (a Temporal.PlainDate), temporalDurationLike (an ECMAScript language value), and options (an ECMAScript language value) and returns either a normal completion containing a Temporal.PlainDate or a throw completion. It adds/subtracts temporalDurationLike to/from temporalDate, returning a point in time that is in the future/past relative to temporalDate. It performs the following steps when called:

1. Let calendar be temporalDate.[[Calendar]].
2. Let duration be ? ToTemporalDuration(temporalDurationLike).
3. If operation is subtract, set duration to CreateNegatedTemporalDuration(duration).
4. Let dateDuration be ToDateDurationRecordWithoutTime(duration).
5. Let resolvedOptions be ? GetOptionsObject(options).
6. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
7. Let result be ? CalendarDateAdd(calendar, temporalDate.[[ISODate]], dateDuration, overflow).
8. Return ! CreateTemporalDate(result, calendar).

# 4 Temporal.PlainTime Objects

A Temporal.PlainTime object is an Object that contains integers corresponding to a particular hour, minute, second, millisecond, microsecond, and nanosecond.

# 4.1 The Temporal.PlainTime Constructor

The Temporal.PlainTime constructor:

- creates and initializes a new Temporal.PlainTime object when called as a constructor.
- is not intended to be called as a function and will throw an exception when called in that manner.
- may be used as the value of an `extends` clause of a class definition. Subclass constructors that intend to inherit the specified Temporal.PlainTime behaviour must include a super call to the %Temporal.PlainTime% constructor to create and initialize subclass instances with the necessary internal slots.

# 4.1.1 Temporal.PlainTime ( [ hour [ , minute [ , second [ , millisecond [ , microsecond [ , nanosecond ] ] ] ] ] ] )

This function performs the following steps when called:

1. If NewTarget is undefined, throw a TypeError exception.
2. If hour is undefined, set hour to 0; else set hour to ? ToIntegerWithTruncation(hour).
3. If minute is undefined, set minute to 0; else set minute to ? ToIntegerWithTruncation(minute).
4. If second is undefined, set second to 0; else set second to ? ToIntegerWithTruncation(second).
5. If millisecond is undefined, set millisecond to 0; else set millisecond to ? ToIntegerWithTruncation(millisecond).
6. If microsecond is undefined, set microsecond to 0; else set microsecond to ? ToIntegerWithTruncation(microsecond).
7. If nanosecond is undefined, set nanosecond to 0; else set nanosecond to ? ToIntegerWithTruncation(nanosecond).
8. If IsValidTime(hour, minute, second, millisecond, microsecond, nanosecond) is false, throw a RangeError exception.
9. Let time be CreateTimeRecord(hour, minute, second, millisecond, microsecond, nanosecond).
10. Return ? CreateTemporalTime(time, NewTarget).

# 4.2 Properties of the Temporal.PlainTime Constructor

The value of the [[Prototype]] internal slot of the Temporal.PlainTime constructor is the intrinsic object %Function.prototype%.

The Temporal.PlainTime constructor has the following properties:

# 4.2.1 Temporal.PlainTime.prototype

The initial value of `Temporal.PlainTime.prototype` is %Temporal.PlainTime.prototype%.

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: false }.

# 4.2.2 Temporal.PlainTime.from ( item [ , options ] )

This function performs the following steps when called:

1. Return ? ToTemporalTime(item, options).

# 4.2.3 Temporal.PlainTime.compare ( one, two )

This function performs the following steps when called:

1. Set one to ? ToTemporalTime(one).
2. Set two to ? ToTemporalTime(two).
3. Return 𝔽(CompareTimeRecord(one.[[Time]], two.[[Time]])).

# 4.3 Properties of the Temporal.PlainTime Prototype Object

The Temporal.PlainTime prototype object

- is itself an ordinary object.
- is not a Temporal.PlainTime instance and does not have a [[InitializedTemporalTime]] internal slot.
- has a [[Prototype]] internal slot whose value is %Object.prototype%.

# 4.3.1 Temporal.PlainTime.prototype.constructor

The initial value of `Temporal.PlainTime.prototype.constructor` is %Temporal.PlainTime%.

# 4.3.2 Temporal.PlainTime.prototype[ %Symbol.toStringTag% ]

The initial value of the %Symbol.toStringTag% property is the String "Temporal.PlainTime".

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }.

# 4.3.3 get Temporal.PlainTime.prototype.hour

`Temporal.PlainTime.prototype.hour` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return 𝔽(plainTime.[[Time]].[[Hour]]).

# 4.3.4 get Temporal.PlainTime.prototype.minute

`Temporal.PlainTime.prototype.minute` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return 𝔽(plainTime.[[Time]].[[Minute]]).

# 4.3.5 get Temporal.PlainTime.prototype.second

`Temporal.PlainTime.prototype.second` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return 𝔽(plainTime.[[Time]].[[Second]]).

# 4.3.6 get Temporal.PlainTime.prototype.millisecond

`Temporal.PlainTime.prototype.millisecond` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return 𝔽(plainTime.[[Time]].[[Millisecond]]).

# 4.3.7 get Temporal.PlainTime.prototype.microsecond

`Temporal.PlainTime.prototype.microsecond` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return 𝔽(plainTime.[[Time]].[[Microsecond]]).

# 4.3.8 get Temporal.PlainTime.prototype.nanosecond

`Temporal.PlainTime.prototype.nanosecond` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return 𝔽(plainTime.[[Time]].[[Nanosecond]]).

# 4.3.9 Temporal.PlainTime.prototype.add ( temporalDurationLike )

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return ? AddDurationToTime(add, plainTime, temporalDurationLike).

# 4.3.10 Temporal.PlainTime.prototype.subtract ( temporalDurationLike )

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return ? AddDurationToTime(subtract, plainTime, temporalDurationLike).

# 4.3.11 Temporal.PlainTime.prototype.with ( temporalTimeLike [ , options ] )

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. If ? IsPartialTemporalObject(temporalTimeLike) is false, throw a TypeError exception.
4. Let partialTime be ? ToTemporalTimeRecord(temporalTimeLike, partial).
5. If partialTime.[[Hour]] is not undefined, then
   1. Let hour be partialTime.[[Hour]].
6. Else,
   1. Let hour be plainTime.[[Time]].[[Hour]].
7. If partialTime.[[Minute]] is not undefined, then
   1. Let minute be partialTime.[[Minute]].
8. Else,
   1. Let minute be plainTime.[[Time]].[[Minute]].
9. If partialTime.[[Second]] is not undefined, then
   1. Let second be partialTime.[[Second]].
10. Else,
    1. Let second be plainTime.[[Time]].[[Second]].
11. If partialTime.[[Millisecond]] is not undefined, then
    1. Let millisecond be partialTime.[[Millisecond]].
12. Else,
    1. Let millisecond be plainTime.[[Time]].[[Millisecond]].
13. If partialTime.[[Microsecond]] is not undefined, then
    1. Let microsecond be partialTime.[[Microsecond]].
14. Else,
    1. Let microsecond be plainTime.[[Time]].[[Microsecond]].
15. If partialTime.[[Nanosecond]] is not undefined, then
    1. Let nanosecond be partialTime.[[Nanosecond]].
16. Else,
    1. Let nanosecond be plainTime.[[Time]].[[Nanosecond]].
17. Let resolvedOptions be ? GetOptionsObject(options).
18. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
19. Let result be ? RegulateTime(hour, minute, second, millisecond, microsecond, nanosecond, overflow).
20. Return ! CreateTemporalTime(result).

# 4.3.12 Temporal.PlainTime.prototype.until ( other [ , options ] )

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return ? DifferenceTemporalPlainTime(until, plainTime, other, options).

# 4.3.13 Temporal.PlainTime.prototype.since ( other [ , options ] )

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return ? DifferenceTemporalPlainTime(since, plainTime, other, options).

# 4.3.14 Temporal.PlainTime.prototype.round ( roundTo )

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. If roundTo is undefined, throw a TypeError exception.
4. If roundTo is a String, then
   1. Let paramString be roundTo.
   2. Set roundTo to OrdinaryObjectCreate(null).
   3. Perform ! CreateDataPropertyOrThrow(roundTo, "smallestUnit", paramString).
5. Else,
   1. Set roundTo to ? GetOptionsObject(roundTo).
6. NOTE: The following steps read options and perform independent validation in alphabetical order (GetRoundingIncrementOption reads "roundingIncrement" and GetRoundingModeOption reads "roundingMode").
7. Let roundingIncrement be ? GetRoundingIncrementOption(roundTo).
8. Let roundingMode be ? GetRoundingModeOption(roundTo, half-expand).
9. Let smallestUnit be ? GetTemporalUnitValuedOption(roundTo, "smallestUnit", required).
10. Perform ? ValidateTemporalUnitValue(smallestUnit, time).
11. Let maximum be MaximumTemporalDurationRoundingIncrement(smallestUnit).
12. Assert: maximum is not unset.
13. Perform ? ValidateTemporalRoundingIncrement(roundingIncrement, maximum, false).
14. Let result be RoundTime(plainTime.[[Time]], roundingIncrement, smallestUnit, roundingMode).
15. Return ! CreateTemporalTime(result).

# 4.3.15 Temporal.PlainTime.prototype.equals ( other )

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Set other to ? ToTemporalTime(other).
4. If CompareTimeRecord(plainTime.[[Time]], other.[[Time]]) = 0, return true.
5. Return false.

# 4.3.16 Temporal.PlainTime.prototype.toString ( [ options ] )

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. NOTE: The following steps read options and perform independent validation in alphabetical order (GetTemporalFractionalSecondDigitsOption reads "fractionalSecondDigits" and GetRoundingModeOption reads "roundingMode").
5. Let digits be ? GetTemporalFractionalSecondDigitsOption(resolvedOptions).
6. Let roundingMode be ? GetRoundingModeOption(resolvedOptions, trunc).
7. Let smallestUnit be ? GetTemporalUnitValuedOption(resolvedOptions, "smallestUnit", unset).
8. Perform ? ValidateTemporalUnitValue(smallestUnit, time).
9. If smallestUnit is hour, throw a RangeError exception.
10. Let precision be ToSecondsStringPrecisionRecord(smallestUnit, digits).
11. Let roundResult be RoundTime(plainTime.[[Time]], precision.[[Increment]], precision.[[Unit]], roundingMode).
12. Return TimeRecordToString(roundResult, precision.[[Precision]]).

# 4.3.17 Temporal.PlainTime.prototype.toLocaleString ( [ locales [ , options ] ] )

An ECMAScript implementation that includes the ECMA-402 Internationalization API must implement this method as specified in the ECMA-402 specification. If an ECMAScript implementation does not include the ECMA-402 API the following specification of this method is used.

The meanings of the optional parameters to this method are defined in the ECMA-402 specification; implementations that do not include ECMA-402 support must not use those parameter positions for anything else.

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return TimeRecordToString(plainTime.[[Time]], auto).

# 4.3.18 Temporal.PlainTime.prototype.toJSON ( )

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Return TimeRecordToString(plainTime.[[Time]], auto).

# 4.3.19 Temporal.PlainTime.prototype.valueOf ( )

This method performs the following steps when called:

1. Throw a TypeError exception.

Note

This method always throws, because in the absence of `valueOf()`, expressions with arithmetic operators such as `plainTime1 > plainTime2` would fall back to being equivalent to `plainTime1.toString() > plainTime2.toString()`. Lexicographical comparison of serialized strings might not seem obviously wrong, because the result would sometimes be correct. Implementations are encouraged to phrase the error message to point users to `Temporal.PlainTime.compare()`, `Temporal.PlainTime.prototype.equals()`, and/or `Temporal.PlainTime.prototype.toString()`.

# 4.4 Properties of Temporal.PlainTime Instances

Temporal.PlainTime instances are ordinary objects that inherit properties from the %Temporal.PlainTime.prototype% intrinsic object. Temporal.PlainTime instances are initially created with the internal slots described in Table 3.

| Table 3: Internal Slots of Temporal.PlainTime Instances Internal Slot | Description                                                                                                |
| --------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| [[InitializedTemporalTime]]                                           | The only specified use of this slot is for distinguishing Temporal.PlainTime instances from other objects. |
| [[Time]]                                                              | A Time Record. The [[Days]] field is ignored.                                                              |

# 4.5 Abstract Operations

# 4.5.1 Time Records

A Time Record is a Record value used to represent a valid clock time, together with a number of overflow days such as might occur in BalanceTime. For any Time Record t, IsValidTime(t.[[Hour]], t.[[Minute]], t.[[Second]], t.[[Millisecond]], t.[[Microsecond]], t.[[Nanosecond]]) must return true.

Time Records have the fields listed in Table 4.

| Table 4: Time Record Fields Field Name | Value                                              | Meaning                        |
| -------------------------------------- | -------------------------------------------------- | ------------------------------ |
| [[Days]]                               | an integer                                         | A number of overflow days.     |
| [[Hour]]                               | an integer in the inclusive interval from 0 to 23  | The number of the hour.        |
| [[Minute]]                             | an integer in the inclusive interval from 0 to 59  | The number of the minute.      |
| [[Second]]                             | an integer in the inclusive interval from 0 to 59  | The number of the second.      |
| [[Millisecond]]                        | an integer in the inclusive interval from 0 to 999 | The number of the millisecond. |
| [[Microsecond]]                        | an integer in the inclusive interval from 0 to 999 | The number of the microsecond. |
| [[Nanosecond]]                         | an integer in the inclusive interval from 0 to 999 | The number of the nanosecond.  |

# 4.5.2 CreateTimeRecord ( hour, minute, second, millisecond, microsecond, nanosecond [ , deltaDays ] )

The abstract operation CreateTimeRecord takes arguments hour (an integer in the inclusive interval from 0 to 23), minute (an integer in the inclusive interval from 0 to 59), second (an integer in the inclusive interval from 0 to 59), millisecond (an integer in the inclusive interval from 0 to 999), microsecond (an integer in the inclusive interval from 0 to 999), and nanosecond (an integer in the inclusive interval from 0 to 999) and optional argument deltaDays (an integer) and returns a Time Record. Most uses of Time Records do not require the deltaDays parameter. It performs the following steps when called:

1. If deltaDays is not present, set deltaDays to 0.
2. Assert: IsValidTime(hour, minute, second, millisecond, microsecond, nanosecond).
3. Return Time Record { [[Days]]: deltaDays, [[Hour]]: hour, [[Minute]]: minute, [[Second]]: second, [[Millisecond]]: millisecond, [[Microsecond]]: microsecond, [[Nanosecond]]: nanosecond }.

# 4.5.3 MidnightTimeRecord ( )

The abstract operation MidnightTimeRecord takes no arguments and returns a Time Record. The returned Record denotes the wall-clock time of midnight. It performs the following steps when called:

1. Return Time Record { [[Days]]: 0, [[Hour]]: 0, [[Minute]]: 0, [[Second]]: 0, [[Millisecond]]: 0, [[Microsecond]]: 0, [[Nanosecond]]: 0 }.

# 4.5.4 NoonTimeRecord ( )

The abstract operation NoonTimeRecord takes no arguments and returns a Time Record. The returned Record denotes the wall-clock time of noon. It performs the following steps when called:

1. Return Time Record { [[Days]]: 0, [[Hour]]: 12, [[Minute]]: 0, [[Second]]: 0, [[Millisecond]]: 0, [[Microsecond]]: 0, [[Nanosecond]]: 0 }.

# 4.5.5 DifferenceTime ( time1, time2 )

The abstract operation DifferenceTime takes arguments time1 (a Time Record) and time2 (a Time Record) and returns a time duration. It returns the elapsed duration from a first wall-clock time, until a second wall-clock time. It performs the following steps when called:

1. Let hours be time2.[[Hour]] \- time1.[[Hour]].
2. Let minutes be time2.[[Minute]] \- time1.[[Minute]].
3. Let seconds be time2.[[Second]] \- time1.[[Second]].
4. Let milliseconds be time2.[[Millisecond]] \- time1.[[Millisecond]].
5. Let microseconds be time2.[[Microsecond]] \- time1.[[Microsecond]].
6. Let nanoseconds be time2.[[Nanosecond]] \- time1.[[Nanosecond]].
7. Let timeDuration be TimeDurationFromComponents(hours, minutes, seconds, milliseconds, microseconds, nanoseconds).
8. Assert: abs(timeDuration) < nsPerDay.
9. Return timeDuration.

# 4.5.6 ToTemporalTime ( item [ , options ] )

The abstract operation ToTemporalTime takes argument item (an ECMAScript language value) and optional argument options (an ECMAScript language value) and returns either a normal completion containing a Temporal.PlainTime or a throw Completion. Converts item to a new Temporal.PlainTime instance if possible, and throws otherwise. It performs the following steps when called:

1. If options is not present, set options to undefined.
2. If item is an Object, then
   1. If item has an [[InitializedTemporalTime]] internal slot, then
      1. Let resolvedOptions be ? GetOptionsObject(options).
      2. Perform ? GetTemporalOverflowOption(resolvedOptions).
      3. Return ! CreateTemporalTime(item.[[Time]]).
   2. If item has an [[InitializedTemporalDateTime]] internal slot, then
      1. Let resolvedOptions be ? GetOptionsObject(options).
      2. Perform ? GetTemporalOverflowOption(resolvedOptions).
      3. Return ! CreateTemporalTime(item.[[ISODateTime]].[[Time]]).
   3. If item has an [[InitializedTemporalZonedDateTime]] internal slot, then
      1. Let isoDateTime be GetISODateTimeFor(item.[[TimeZone]], item.[[EpochNanoseconds]]).
      2. Let resolvedOptions be ? GetOptionsObject(options).
      3. Perform ? GetTemporalOverflowOption(resolvedOptions).
      4. Return ! CreateTemporalTime(isoDateTime.[[Time]]).
   4. Let result be ? ToTemporalTimeRecord(item).
   5. Let resolvedOptions be ? GetOptionsObject(options).
   6. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
   7. Set result to ? RegulateTime(result.[[Hour]], result.[[Minute]], result.[[Second]], result.[[Millisecond]], result.[[Microsecond]], result.[[Nanosecond]], overflow).
3. Else,
   1. If item is not a String, throw a TypeError exception.
   2. Let parseResult be ? ParseISODateTime(item, « TemporalTimeString »).
   3. Assert: parseResult.[[Time]] is not start-of-day.
   4. Set result to parseResult.[[Time]].
   5. NOTE: A successful parse using TemporalTimeString guarantees absence of ambiguity with respect to any ISO 8601 date-only, year-month, or month-day representation.
   6. Let resolvedOptions be ? GetOptionsObject(options).
   7. Perform ? GetTemporalOverflowOption(resolvedOptions).
4. Return ! CreateTemporalTime(result).

# 4.5.7 ToTimeRecordOrMidnight ( item )

The abstract operation ToTimeRecordOrMidnight takes argument item (an ECMAScript language value) and returns either a normal completion containing a Time Record or a throw completion. Converts item to a Time Record if possible, considering undefined to be the same as midnight, and throws otherwise. It performs the following steps when called:

1. If item is undefined, return MidnightTimeRecord().
2. Let plainTime be ? ToTemporalTime(item).
3. Return plainTime.[[Time]].

# 4.5.8 RegulateTime ( hour, minute, second, millisecond, microsecond, nanosecond, overflow )

The abstract operation RegulateTime takes arguments hour (an integer), minute (an integer), second (an integer), millisecond (an integer), microsecond (an integer), nanosecond (an integer), and overflow (either constrain or reject) and returns either a normal completion containing a Time Record or a throw completion. It applies the correction given by overflow to the given time. If overflow is constrain, out-of-range values are clamped. If overflow is reject, a RangeError is thrown if any values are out of range. It performs the following steps when called:

1. If overflow is constrain, then
   1. Set hour to the result of clamping hour between 0 and 23.
   2. Set minute to the result of clamping minute between 0 and 59.
   3. Set second to the result of clamping second between 0 and 59.
   4. Set millisecond to the result of clamping millisecond between 0 and 999.
   5. Set microsecond to the result of clamping microsecond between 0 and 999.
   6. Set nanosecond to the result of clamping nanosecond between 0 and 999.
2. Else,
   1. Assert: overflow is reject.
   2. If IsValidTime(hour, minute, second, millisecond, microsecond, nanosecond) is false, throw a RangeError exception.
3. Return CreateTimeRecord(hour, minute, second, millisecond, microsecond,nanosecond).

# 4.5.9 IsValidTime ( hour, minute, second, millisecond, microsecond, nanosecond )

The abstract operation IsValidTime takes arguments hour (an integer), minute (an integer), second (an integer), millisecond (an integer), microsecond (an integer), and nanosecond (an integer) and returns a Boolean. The return value is true if its arguments form a valid time of day, and false otherwise. Leap seconds are not taken into account. It performs the following steps when called:

1. If hour < 0 or hour > 23, return false.
2. If minute < 0 or minute > 59, return false.
3. If second < 0 or second > 59, return false.
4. If millisecond < 0 or millisecond > 999, return false.
5. If microsecond < 0 or microsecond > 999, return false.
6. If nanosecond < 0 or nanosecond > 999, return false.
7. Return true.

# 4.5.10 BalanceTime ( hour, minute, second, millisecond, microsecond, nanosecond )

The abstract operation BalanceTime takes arguments hour (an integer), minute (an integer), second (an integer), millisecond (an integer), microsecond (an integer), and nanosecond (an integer) and returns a Time Record. It balances the given time components so that each falls within its conventional range, carrying over excess into the next larger unit, and returns the result as a Time Record. It performs the following steps when called:

1. Set microsecond to microsecond \+ floor(nanosecond / 1000).
2. Set nanosecond to nanosecond modulo 1000.
3. Set millisecond to millisecond \+ floor(microsecond / 1000).
4. Set microsecond to microsecond modulo 1000.
5. Set second to second \+ floor(millisecond / 1000).
6. Set millisecond to millisecond modulo 1000.
7. Set minute to minute \+ floor(second / 60).
8. Set second to second modulo 60.
9. Set hour to hour \+ floor(minute / 60).
10. Set minute to minute modulo 60.
11. Let deltaDays be floor(hour / 24).
12. Set hour to hour modulo 24.
13. Return CreateTimeRecord(hour, minute, second, millisecond, microsecond, nanosecond, deltaDays).

# 4.5.11 CreateTemporalTime ( time [ , newTarget ] )

The abstract operation CreateTemporalTime takes argument time (a Time Record) and optional argument newTarget (a constructor) and returns either a normal completion containing a Temporal.PlainTime or a throw completion. It creates a new Temporal.PlainTime instance and fills the internal slots with valid values. It performs the following steps when called:

1. If newTarget is not present, set newTarget to %Temporal.PlainTime%.
2. Let object be ? OrdinaryCreateFromConstructor(newTarget, "%Temporal.PlainTime.prototype%", « [[InitializedTemporalTime]], [[Time]] »).
3. Set object.[[Time]] to time.
4. Return object.

# 4.5.12 ToTemporalTimeRecord ( temporalTimeLike [ , completeness ] )

The abstract operation ToTemporalTimeRecord takes argument temporalTimeLike (an Object) and optional argument completeness (either partial or complete) and returns either a normal completion containing a TemporalTimeLike Record or a throw completion. It converts temporalTimeLike to a TemporalTimeLike Record by reading time component properties, with missing components set to 0 if completeness is complete or to unset if partial. It throws a TypeError if temporalTimeLike has no recognized time component properties. It performs the following steps when called:

1. If completeness is not present, set completeness to complete.
2. If completeness is complete, then
   1. Let result be a new TemporalTimeLike Record with each field set to 0.
3. Else,
   1. Let result be a new TemporalTimeLike Record with each field set to unset.
4. Let any be false.
5. Let hour be ? Get(temporalTimeLike, "hour").
6. If hour is not undefined, then
   1. Set result.[[Hour]] to ? ToIntegerWithTruncation(hour).
   2. Set any to true.
7. Let microsecond be ? Get(temporalTimeLike, "microsecond").
8. If microsecond is not undefined, then
   1. Set result.[[Microsecond]] to ? ToIntegerWithTruncation(microsecond).
   2. Set any to true.
9. Let millisecond be ? Get(temporalTimeLike, "millisecond").
10. If millisecond is not undefined, then
    1. Set result.[[Millisecond]] to ? ToIntegerWithTruncation(millisecond).
    2. Set any to true.
11. Let minute be ? Get(temporalTimeLike, "minute").
12. If minute is not undefined, then
    1. Set result.[[Minute]] to ? ToIntegerWithTruncation(minute).
    2. Set any to true.
13. Let nanosecond be ? Get(temporalTimeLike, "nanosecond").
14. If nanosecond is not undefined, then
    1. Set result.[[Nanosecond]] to ? ToIntegerWithTruncation(nanosecond).
    2. Set any to true.
15. Let second be ? Get(temporalTimeLike, "second").
16. If second is not undefined, then
    1. Set result.[[Second]] to ? ToIntegerWithTruncation(second).
    2. Set any to true.
17. If any is false, throw a TypeError exception.
18. Return result.

| Table 5: TemporalTimeLike Record Fields Field Name | Property Name |
| -------------------------------------------------- | ------------- |
| [[Hour]]                                           | "hour"        |
| [[Minute]]                                         | "minute"      |
| [[Second]]                                         | "second"      |
| [[Millisecond]]                                    | "millisecond" |
| [[Microsecond]]                                    | "microsecond" |
| [[Nanosecond]]                                     | "nanosecond"  |

# 4.5.13 TimeRecordToString ( time, precision )

The abstract operation TimeRecordToString takes arguments time (a Time Record) and precision (either an integer in the inclusive interval from 0 to 9, minute, or auto) and returns a String. It formats the given time as an ISO 8601 string, to the precision specified by precision. It performs the following steps when called:

1. Let subSecondNanoseconds be time.[[Millisecond]] × 106 \+ time.[[Microsecond]] × 103 \+ time.[[Nanosecond]].
2. Return FormatTimeString(time.[[Hour]], time.[[Minute]], time.[[Second]], subSecondNanoseconds, precision).

# 4.5.14 CompareTimeRecord ( time1, time2 )

The abstract operation CompareTimeRecord takes arguments time1 (a Time Record) and time2 (a Time Record) and returns one of -1, 0, or 1. It compares the two given times and returns -1 if the second comes earlier in the day than the first, 1 if the first comes earlier in the day than the second, and 0 if they are the same. It performs the following steps when called:

1. If time1.[[Hour]] > time2.[[Hour]], return 1.
2. If time1.[[Hour]] < time2.[[Hour]], return -1.
3. If time1.[[Minute]] > time2.[[Minute]], return 1.
4. If time1.[[Minute]] < time2.[[Minute]], return -1.
5. If time1.[[Second]] > time2.[[Second]], return 1.
6. If time1.[[Second]] < time2.[[Second]], return -1.
7. If time1.[[Millisecond]] > time2.[[Millisecond]], return 1.
8. If time1.[[Millisecond]] < time2.[[Millisecond]], return -1.
9. If time1.[[Microsecond]] > time2.[[Microsecond]], return 1.
10. If time1.[[Microsecond]] < time2.[[Microsecond]], return -1.
11. If time1.[[Nanosecond]] > time2.[[Nanosecond]], return 1.
12. If time1.[[Nanosecond]] < time2.[[Nanosecond]], return -1.
13. Return 0.

# 4.5.15 AddTime ( time, timeDuration )

The abstract operation AddTime takes arguments time (a Time Record) and timeDuration (a time duration) and returns a Time Record. It adds timeDuration to time and returns the balanced result. It performs the following steps when called:

1. Return BalanceTime(time.[[Hour]], time.[[Minute]], time.[[Second]], time.[[Millisecond]], time.[[Microsecond]], time.[[Nanosecond]] \+ timeDuration).
2. NOTE: If using floating points to implement this operation, add the time components separately before balancing to avoid errors with unsafe integers.

# 4.5.16 RoundTime ( time, increment, unit, roundingMode )

The abstract operation RoundTime takes arguments time (a Time Record), increment (a positive integer), unit (either a time unit or day), and roundingMode (a rounding mode) and returns a Time Record. It rounds a time to the given increment. It performs the following steps when called:

1. If unit is either day or hour, then
   1. Let quantity be ((((time.[[Hour]] × 60 + time.[[Minute]]) × 60 + time.[[Second]]) × 1000 + time.[[Millisecond]]) × 1000 + time.[[Microsecond]]) × 1000 + time.[[Nanosecond]].
2. Else if unit is minute, then
   1. Let quantity be (((time.[[Minute]] × 60 + time.[[Second]]) × 1000 + time.[[Millisecond]]) × 1000 + time.[[Microsecond]]) × 1000 + time.[[Nanosecond]].
3. Else if unit is second, then
   1. Let quantity be ((time.[[Second]] × 1000 + time.[[Millisecond]]) × 1000 + time.[[Microsecond]]) × 1000 + time.[[Nanosecond]].
4. Else if unit is millisecond, then
   1. Let quantity be (time.[[Millisecond]] × 1000 + time.[[Microsecond]]) × 1000 + time.[[Nanosecond]].
5. Else if unit is microsecond, then
   1. Let quantity be time.[[Microsecond]] × 1000 + time.[[Nanosecond]].
6. Else,
   1. Assert: unit is nanosecond.
   2. Let quantity be time.[[Nanosecond]].
7. Let unitLength be the value in the "Length in Nanoseconds" column of the row of Table 21 whose "Value" column contains unit.
8. Let result be RoundNumberToIncrement(quantity, increment × unitLength, roundingMode) / unitLength.
9. If unit is day, return CreateTimeRecord(0, 0, 0, 0, 0, 0, result).
10. If unit is hour, return BalanceTime(result, 0, 0, 0, 0, 0).
11. If unit is minute, return BalanceTime(time.[[Hour]], result, 0, 0, 0, 0).
12. If unit is second, return BalanceTime(time.[[Hour]], time.[[Minute]], result, 0, 0, 0).
13. If unit is millisecond, return BalanceTime(time.[[Hour]], time.[[Minute]], time.[[Second]], result, 0, 0).
14. If unit is microsecond, return BalanceTime(time.[[Hour]], time.[[Minute]], time.[[Second]], time.[[Millisecond]], result, 0).
15. Assert: unit is nanosecond.
16. Return BalanceTime(time.[[Hour]], time.[[Minute]], time.[[Second]], time.[[Millisecond]], time.[[Microsecond]], result).

# 4.5.17 DifferenceTemporalPlainTime ( operation, temporalTime, other, options )

The abstract operation DifferenceTemporalPlainTime takes arguments operation (either since or until), temporalTime (a Temporal.PlainTime), other (an ECMAScript language value), and options (an ECMAScript language value) and returns either a normal completion containing a Temporal.Duration or a throw completion. It computes the difference between the two times represented by temporalTime and other, optionally rounds it, and returns it as a Temporal.Duration object. It performs the following steps when called:

1. Set other to ? ToTemporalTime(other).
2. Let resolvedOptions be ? GetOptionsObject(options).
3. Let settings be ? GetDifferenceSettings(operation, resolvedOptions, time, « », nanosecond, hour).
4. Let timeDuration be DifferenceTime(temporalTime.[[Time]], other.[[Time]]).
5. Set timeDuration to ! RoundTimeDuration(timeDuration, settings.[[RoundingIncrement]], settings.[[SmallestUnit]], settings.[[RoundingMode]]).
6. Let duration be CombineDateAndTimeDuration(ZeroDateDuration(), timeDuration).
7. Let result be ! TemporalDurationFromInternal(duration, settings.[[LargestUnit]]).
8. If operation is since, set result to CreateNegatedTemporalDuration(result).
9. Return result.

# 4.5.18 AddDurationToTime ( operation, temporalTime, temporalDurationLike )

The abstract operation AddDurationToTime takes arguments operation (either add or subtract), temporalTime (a Temporal.PlainTime), and temporalDurationLike (an ECMAScript language value) and returns either a normal completion containing a Temporal.PlainTime or a throw completion. It adds/subtracts temporalDurationLike to/from temporalTime, returning a point in time that is in the future/past relative to temporalTime. It performs the following steps when called:

1. Let duration be ? ToTemporalDuration(temporalDurationLike).
2. If operation is subtract, set duration to CreateNegatedTemporalDuration(duration).
3. Let internalDuration be ToInternalDurationRecord(duration).
4. Let result be AddTime(temporalTime.[[Time]], internalDuration.[[Time]]).
5. Return ! CreateTemporalTime(result).

# 5 Temporal.PlainDateTime Objects

A Temporal.PlainDateTime object is an Object that contains integers corresponding to a particular year, month, day, hour, minute, second, millisecond, microsecond, and nanosecond.

# 5.1 The Temporal.PlainDateTime Constructor

The Temporal.PlainDateTime constructor:

- creates and initializes a new Temporal.PlainDateTime object when called as a constructor.
- is not intended to be called as a function and will throw an exception when called in that manner.
- may be used as the value of an `extends` clause of a class definition. Subclass constructors that intend to inherit the specified Temporal.PlainDateTime behaviour must include a super call to the %Temporal.PlainDateTime% constructor to create and initialize subclass instances with the necessary internal slots.

# 5.1.1 Temporal.PlainDateTime ( isoYear, isoMonth, isoDay [ , hour [ , minute [ , second [ , millisecond [ , microsecond [ , nanosecond [ , calendar ] ] ] ] ] ] ] )

This function performs the following steps when called:

1. If NewTarget is undefined, throw a TypeError exception.
2. Set isoYear to ? ToIntegerWithTruncation(isoYear).
3. Set isoMonth to ? ToIntegerWithTruncation(isoMonth).
4. Set isoDay to ? ToIntegerWithTruncation(isoDay).
5. If hour is undefined, set hour to 0; else set hour to ? ToIntegerWithTruncation(hour).
6. If minute is undefined, set minute to 0; else set minute to ? ToIntegerWithTruncation(minute).
7. If second is undefined, set second to 0; else set second to ? ToIntegerWithTruncation(second).
8. If millisecond is undefined, set millisecond to 0; else set millisecond to ? ToIntegerWithTruncation(millisecond).
9. If microsecond is undefined, set microsecond to 0; else set microsecond to ? ToIntegerWithTruncation(microsecond).
10. If nanosecond is undefined, set nanosecond to 0; else set nanosecond to ? ToIntegerWithTruncation(nanosecond).
11. If calendar is undefined, set calendar to "iso8601".
12. If calendar is not a String, throw a TypeError exception.
13. Set calendar to ? CanonicalizeCalendar(calendar).
14. If IsValidISODate(isoYear, isoMonth, isoDay) is false, throw a RangeError exception.
15. Let isoDate be CreateISODateRecord(isoYear, isoMonth, isoDay).
16. If IsValidTime(hour, minute, second, millisecond, microsecond, nanosecond) is false, throw a RangeError exception.
17. Let time be CreateTimeRecord(hour, minute, second, millisecond, microsecond, nanosecond).
18. Let isoDateTime be CombineISODateAndTimeRecord(isoDate, time).
19. Return ? CreateTemporalDateTime(isoDateTime, calendar, NewTarget).

# 5.2 Properties of the Temporal.PlainDateTime Constructor

The value of the [[Prototype]] internal slot of the Temporal.PlainDateTime constructor is the intrinsic object %Function.prototype%.

The Temporal.PlainDateTime constructor has the following properties:

# 5.2.1 Temporal.PlainDateTime.prototype

The initial value of `Temporal.PlainDateTime.prototype` is %Temporal.PlainDateTime.prototype%.

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: false }.

# 5.2.2 Temporal.PlainDateTime.from ( item [ , options ] )

This function performs the following steps when called:

1. Return ? ToTemporalDateTime(item, options).

# 5.2.3 Temporal.PlainDateTime.compare ( one, two )

This function performs the following steps when called:

1. Set one to ? ToTemporalDateTime(one).
2. Set two to ? ToTemporalDateTime(two).
3. Return 𝔽(CompareISODateTime(one.[[ISODateTime]], two.[[ISODateTime]])).

# 5.3 Properties of the Temporal.PlainDateTime Prototype Object

The Temporal.PlainDateTime prototype object

- is the intrinsic object %Temporal.PlainDateTime.prototype%.
- is itself an ordinary object.
- is not a Temporal.PlainDateTime instance and does not have a [[InitializedTemporalDateTime]] internal slot.
- has a [[Prototype]] internal slot whose value is %Object.prototype%.

Note

An ECMAScript implementation that includes the ECMA-402 Internationalization API extends this prototype with additional properties in order to represent calendar data.

# 5.3.1 Temporal.PlainDateTime.prototype.constructor

The initial value of `Temporal.PlainDateTime.prototype.constructor` is %Temporal.PlainDateTime%.

# 5.3.2 Temporal.PlainDateTime.prototype[ %Symbol.toStringTag% ]

The initial value of the %Symbol.toStringTag% property is the String "Temporal.PlainDateTime".

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }.

# 5.3.3 get Temporal.PlainDateTime.prototype.calendarId

`Temporal.PlainDateTime.prototype.calendarId` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return plainDateTime.[[Calendar]].

# 5.3.4 get Temporal.PlainDateTime.prototype.era

`Temporal.PlainDate.prototype.era` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[Era]].

# 5.3.5 get Temporal.PlainDateTime.prototype.eraYear

`Temporal.PlainDateTime.prototype.eraYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Let result be CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[EraYear]].
4. If result is undefined, return undefined.
5. Return 𝔽(result).

# 5.3.6 get Temporal.PlainDateTime.prototype.year

`Temporal.PlainDateTime.prototype.year` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[Year]]).

# 5.3.7 get Temporal.PlainDateTime.prototype.month

`Temporal.PlainDateTime.prototype.month` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[Month]]).

# 5.3.8 get Temporal.PlainDateTime.prototype.monthCode

`Temporal.PlainDateTime.prototype.monthCode` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[MonthCode]].

# 5.3.9 get Temporal.PlainDateTime.prototype.day

`Temporal.PlainDateTime.prototype.day` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[Day]]).

# 5.3.10 get Temporal.PlainDateTime.prototype.hour

`Temporal.PlainDateTime.prototype.hour` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(plainDateTime.[[ISODateTime]].[[Time]].[[Hour]]).

# 5.3.11 get Temporal.PlainDateTime.prototype.minute

`Temporal.PlainDateTime.prototype.minute` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(plainDateTime.[[ISODateTime]].[[Time]].[[Minute]]).

# 5.3.12 get Temporal.PlainDateTime.prototype.second

`Temporal.PlainDateTime.prototype.second` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(plainDateTime.[[ISODateTime]].[[Time]].[[Second]]).

# 5.3.13 get Temporal.PlainDateTime.prototype.millisecond

`Temporal.PlainDateTime.prototype.millisecond` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(plainDateTime.[[ISODateTime]].[[Time]].[[Millisecond]]).

# 5.3.14 get Temporal.PlainDateTime.prototype.microsecond

`Temporal.PlainDateTime.prototype.microsecond` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(plainDateTime.[[ISODateTime]].[[Time]].[[Microsecond]]).

# 5.3.15 get Temporal.PlainDateTime.prototype.nanosecond

`Temporal.PlainDateTime.prototype.nanosecond` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(plainDateTime.[[ISODateTime]].[[Time]].[[Nanosecond]]).

# 5.3.16 get Temporal.PlainDateTime.prototype.dayOfWeek

`Temporal.PlainDateTime.prototype.dayOfWeek` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[DayOfWeek]]).

# 5.3.17 get Temporal.PlainDateTime.prototype.dayOfYear

`Temporal.PlainDateTime.prototype.dayOfYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[DayOfYear]]).

# 5.3.18 get Temporal.PlainDateTime.prototype.weekOfYear

`Temporal.PlainDateTime.prototype.weekOfYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Let result be CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[WeekOfYear]].[[Week]].
4. If result is undefined, return undefined.
5. Return 𝔽(result).

# 5.3.19 get Temporal.PlainDateTime.prototype.yearOfWeek

`Temporal.PlainDateTime.prototype.yearOfWeek` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Let result be CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[WeekOfYear]].[[Year]].
4. If result is undefined, return undefined.
5. Return 𝔽(result).

# 5.3.20 get Temporal.PlainDateTime.prototype.daysInWeek

`Temporal.PlainDateTime.prototype.daysInWeek` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[DaysInWeek]]).

# 5.3.21 get Temporal.PlainDateTime.prototype.daysInMonth

`Temporal.PlainDateTime.prototype.daysInMonth` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[DaysInMonth]]).

# 5.3.22 get Temporal.PlainDateTime.prototype.daysInYear

`Temporal.PlainDateTime.prototype.daysInYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[DaysInYear]]).

# 5.3.23 get Temporal.PlainDateTime.prototype.monthsInYear

`Temporal.PlainDateTime.prototype.monthsInYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return 𝔽(CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[MonthsInYear]]).

# 5.3.24 get Temporal.PlainDateTime.prototype.inLeapYear

`Temporal.PlainDateTime.prototype.inLeapYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return CalendarISOToDate(plainDateTime.[[Calendar]], plainDateTime.[[ISODateTime]].[[ISODate]]).[[InLeapYear]].

# 5.3.25 Temporal.PlainDateTime.prototype.with ( temporalDateTimeLike [ , options ] )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. If ? IsPartialTemporalObject(temporalDateTimeLike) is false, throw a TypeError exception.
4. Let calendar be plainDateTime.[[Calendar]].
5. Let fields be ISODateToFields(calendar, plainDateTime.[[ISODateTime]].[[ISODate]], date).
6. Set fields.[[Hour]] to plainDateTime.[[ISODateTime]].[[Time]].[[Hour]].
7. Set fields.[[Minute]] to plainDateTime.[[ISODateTime]].[[Time]].[[Minute]].
8. Set fields.[[Second]] to plainDateTime.[[ISODateTime]].[[Time]].[[Second]].
9. Set fields.[[Millisecond]] to plainDateTime.[[ISODateTime]].[[Time]].[[Millisecond]].
10. Set fields.[[Microsecond]] to plainDateTime.[[ISODateTime]].[[Time]].[[Microsecond]].
11. Set fields.[[Nanosecond]] to plainDateTime.[[ISODateTime]].[[Time]].[[Nanosecond]].
12. Let partialDateTime be ? PrepareCalendarFields(calendar, temporalDateTimeLike, « year, month, month-code, day », « hour, minute, second, millisecond, microsecond, nanosecond », partial).
13. Set fields to CalendarMergeFields(calendar, fields, partialDateTime).
14. Let resolvedOptions be ? GetOptionsObject(options).
15. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
16. Let result be ? InterpretTemporalDateTimeFields(calendar, fields, overflow).
17. Return ? CreateTemporalDateTime(result, calendar).

# 5.3.26 Temporal.PlainDateTime.prototype.withPlainTime ( [ plainTimeLike ] )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Let time be ? ToTimeRecordOrMidnight(plainTimeLike).
4. Let isoDateTime be CombineISODateAndTimeRecord(plainDateTime.[[ISODateTime]].[[ISODate]], time).
5. Return ? CreateTemporalDateTime(isoDateTime, plainDateTime.[[Calendar]]).

# 5.3.27 Temporal.PlainDateTime.prototype.withCalendar ( calendarLike )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Let calendar be ? ToTemporalCalendarIdentifier(calendarLike).
4. Return ! CreateTemporalDateTime(plainDateTime.[[ISODateTime]], calendar).

# 5.3.28 Temporal.PlainDateTime.prototype.add ( temporalDurationLike [ , options ] )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return ? AddDurationToDateTime(add, plainDateTime, temporalDurationLike, options).

# 5.3.29 Temporal.PlainDateTime.prototype.subtract ( temporalDurationLike [ , options ] )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return ? AddDurationToDateTime(subtract, plainDateTime, temporalDurationLike, options).

# 5.3.30 Temporal.PlainDateTime.prototype.until ( other [ , options ] )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return ? DifferenceTemporalPlainDateTime(until, plainDateTime, other, options).

# 5.3.31 Temporal.PlainDateTime.prototype.since ( other [ , options ] )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return ? DifferenceTemporalPlainDateTime(since, plainDateTime, other, options).

# 5.3.32 Temporal.PlainDateTime.prototype.round ( roundTo )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. If roundTo is undefined, throw a TypeError exception.
4. If roundTo is a String, then
   1. Let paramString be roundTo.
   2. Set roundTo to OrdinaryObjectCreate(null).
   3. Perform ! CreateDataPropertyOrThrow(roundTo, "smallestUnit", paramString).
5. Else,
   1. Set roundTo to ? GetOptionsObject(roundTo).
6. NOTE: The following steps read options and perform independent validation in alphabetical order (GetRoundingIncrementOption reads "roundingIncrement" and GetRoundingModeOption reads "roundingMode").
7. Let roundingIncrement be ? GetRoundingIncrementOption(roundTo).
8. Let roundingMode be ? GetRoundingModeOption(roundTo, half-expand).
9. Let smallestUnit be ? GetTemporalUnitValuedOption(roundTo, "smallestUnit", required).
10. Perform ? ValidateTemporalUnitValue(smallestUnit, time, « day »).
11. If smallestUnit is day, then
    1. Let maximum be 1.
    2. Let inclusive be true.
12. Else,
    1. Let maximum be MaximumTemporalDurationRoundingIncrement(smallestUnit).
    2. Assert: maximum is not unset.
    3. Let inclusive be false.
13. Perform ? ValidateTemporalRoundingIncrement(roundingIncrement, maximum, inclusive).
14. If smallestUnit is nanosecond and roundingIncrement = 1, then
    1. Return ! CreateTemporalDateTime(plainDateTime.[[ISODateTime]], plainDateTime.[[Calendar]]).
15. Let result be RoundISODateTime(plainDateTime.[[ISODateTime]], roundingIncrement, smallestUnit, roundingMode).
16. Return ? CreateTemporalDateTime(result, plainDateTime.[[Calendar]]).

# 5.3.33 Temporal.PlainDateTime.prototype.equals ( other )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Set other to ? ToTemporalDateTime(other).
4. If CompareISODateTime(plainDateTime.[[ISODateTime]], other.[[ISODateTime]]) ≠ 0, return false.
5. Return CalendarEquals(plainDateTime.[[Calendar]], other.[[Calendar]]).

# 5.3.34 Temporal.PlainDateTime.prototype.toString ( [ options ] )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. NOTE: The following steps read options and perform independent validation in alphabetical order (GetTemporalShowCalendarNameOption reads "calendarName", GetTemporalFractionalSecondDigitsOption reads "fractionalSecondDigits", and GetRoundingModeOption reads "roundingMode").
5. Let showCalendar be ? GetTemporalShowCalendarNameOption(resolvedOptions).
6. Let digits be ? GetTemporalFractionalSecondDigitsOption(resolvedOptions).
7. Let roundingMode be ? GetRoundingModeOption(resolvedOptions, trunc).
8. Let smallestUnit be ? GetTemporalUnitValuedOption(resolvedOptions, "smallestUnit", unset).
9. Perform ? ValidateTemporalUnitValue(smallestUnit, time).
10. If smallestUnit is hour, throw a RangeError exception.
11. Let precision be ToSecondsStringPrecisionRecord(smallestUnit, digits).
12. Let result be RoundISODateTime(plainDateTime.[[ISODateTime]], precision.[[Increment]], precision.[[Unit]], roundingMode).
13. If ISODateTimeWithinLimits(result) is false, throw a RangeError exception.
14. Return ISODateTimeToString(result, plainDateTime.[[Calendar]], precision.[[Precision]], showCalendar).

# 5.3.35 Temporal.PlainDateTime.prototype.toLocaleString ( [ locales [ , options ] ] )

An ECMAScript implementation that includes the ECMA-402 Internationalization API must implement this method as specified in the ECMA-402 specification. If an ECMAScript implementation does not include the ECMA-402 API the following specification of this method is used.

The meanings of the optional parameters to this method are defined in the ECMA-402 specification; implementations that do not include ECMA-402 support must not use those parameter positions for anything else.

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return ISODateTimeToString(plainDateTime.[[ISODateTime]], plainDateTime.[[Calendar]], auto, auto).

# 5.3.36 Temporal.PlainDateTime.prototype.toJSON ( )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return ISODateTimeToString(plainDateTime.[[ISODateTime]], plainDateTime.[[Calendar]], auto, auto).

# 5.3.37 Temporal.PlainDateTime.prototype.valueOf ( )

This method performs the following steps when called:

1. Throw a TypeError exception.

Note

This method always throws, because in the absence of `valueOf()`, expressions with arithmetic operators such as `plainDateTime1 > plainDateTime2` would fall back to being equivalent to `plainDateTime1.toString() > plainDateTime2.toString()`. Lexicographical comparison of serialized strings might not seem obviously wrong, because the result would sometimes be correct. Implementations are encouraged to phrase the error message to point users to `Temporal.PlainDateTime.compare()`, `Temporal.PlainDateTime.prototype.equals()`, and/or `Temporal.PlainDateTime.prototype.toString()`.

# 5.3.38 Temporal.PlainDateTime.prototype.toZonedDateTime ( temporalTimeZoneLike [ , options ] )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Let timeZone be ? ToTemporalTimeZoneIdentifier(temporalTimeZoneLike).
4. Let resolvedOptions be ? GetOptionsObject(options).
5. Let disambiguation be ? GetTemporalDisambiguationOption(resolvedOptions).
6. Let epochNs be ? GetEpochNanosecondsFor(timeZone, plainDateTime.[[ISODateTime]], disambiguation).
7. Return ! CreateTemporalZonedDateTime(epochNs, timeZone, plainDateTime.[[Calendar]]).

# 5.3.39 Temporal.PlainDateTime.prototype.toPlainDate ( )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return ! CreateTemporalDate(plainDateTime.[[ISODateTime]].[[ISODate]], plainDateTime.[[Calendar]]).

# 5.3.40 Temporal.PlainDateTime.prototype.toPlainTime ( )

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Return ! CreateTemporalTime(plainDateTime.[[ISODateTime]].[[Time]]).

# 5.4 Properties of Temporal.PlainDateTime Instances

Temporal.PlainDateTime instances are ordinary objects that inherit properties from the %Temporal.PlainDateTime.prototype% intrinsic object. Temporal.PlainDateTime instances are initially created with the internal slots described in Table 6.

| Table 6: Internal Slots of Temporal.PlainDateTime Instances Internal Slot | Description                                                                                                    |
| ------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| [[InitializedTemporalDateTime]]                                           | The only specified use of this slot is for distinguishing Temporal.PlainDateTime instances from other objects. |
| [[ISODateTime]]                                                           | An ISO Date-Time Record.                                                                                       |
| [[Calendar]]                                                              | A calendar type.                                                                                               |

# 5.5 Abstract Operations

# 5.5.1 ISO Date-Time Records

An ISO Date-Time Record is a Record value used to represent a valid calendar date in the ISO 8601 calendar together with a clock time. For any ISO Date-Time Record r, IsValidISODate(r.[[ISODate]].[[Year]], r.[[ISODate]].[[Month]], r.[[ISODate]].[[Day]]) must return true, and IsValidTime(r.[[Time]].[[Hour]], r.[[Time]].[[Minute]], r.[[Time]].[[Second]], r.[[Time]].[[Millisecond]], r.[[Time]].[[Microsecond]], r.[[Time]].[[Nanosecond]]) must return true. It is not necessary for ISODateTimeWithinLimits(r) to return true.

ISO Date-Time Records have the fields listed in Table 7.

| Table 7: ISO Date-Time Record Fields Field Name | Value              | Meaning                                  |
| ----------------------------------------------- | ------------------ | ---------------------------------------- |
| [[ISODate]]                                     | an ISO Date Record | The date in the ISO 8601 calendar.       |
| [[Time]]                                        | a Time Record      | The time. The [[Days]] field is ignored. |

# 5.5.2 TimeValueToISODateTimeRecord ( t )

The abstract operation TimeValueToISODateTimeRecord takes argument t (a finite time value) and returns an ISO Date-Time Record. It converts a time value into an ISO Date-Time Record. It performs the following steps when called:

1. Let isoDate be CreateISODateRecord(ℝ(YearFromTime(t)), ℝ(MonthFromTime(t)) + 1, ℝ(DateFromTime(t))).
2. Let time be CreateTimeRecord(ℝ(HourFromTime(t)), ℝ(MinFromTime(t)), ℝ(SecFromTime(t)), ℝ(msFromTime(t)), 0, 0).
3. Return ISO Date-Time Record { [[ISODate]]: isoDate, [[Time]]: time }.

# 5.5.3 CombineISODateAndTimeRecord ( isoDate, time )

The abstract operation CombineISODateAndTimeRecord takes arguments isoDate (an ISO Date Record) and time (a Time Record) and returns an ISO Date-Time Record. It combines a date and a time into one ISO Date-Time Record. It performs the following steps when called:

1. NOTE: time.[[Days]] is ignored.
2. Return ISO Date-Time Record { [[ISODate]]: isoDate, [[Time]]: time }.

# 5.5.4 ISODateTimeWithinLimits ( isoDateTime )

The abstract operation ISODateTimeWithinLimits takes argument isoDateTime (an ISO Date-Time Record) and returns a Boolean. The return value is true if the combination of a date in the ISO 8601 calendar with a wall-clock time, given by the arguments, is within the representable range of `Temporal.PlainDateTime`, and false otherwise.

Note

Temporal.PlainDateTime objects can represent points in time within 24 hours (8.64 × 1013 nanoseconds) of the Temporal.Instant boundaries. This ensures that a Temporal.Instant object can be converted into a Temporal.PlainDateTime object using any time zone.

It performs the following steps when called:

1. If abs(ISODateToEpochDays(isoDateTime.[[ISODate]].[[Year]], isoDateTime.[[ISODate]].[[Month]] \- 1, isoDateTime.[[ISODate]].[[Day]])) > 108 \+ 1, return false.
2. Let ns be ℝ(GetUTCEpochNanoseconds(isoDateTime)).
3. If ns ≤ nsMinInstant \- nsPerDay, return false.
4. If ns ≥ nsMaxInstant \+ nsPerDay, return false.
5. Return true.

# 5.5.5 InterpretTemporalDateTimeFields ( calendar, fields, overflow )

The abstract operation InterpretTemporalDateTimeFields takes arguments calendar (a calendar type), fields (a Calendar Fields Record), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date-Time Record, or a throw completion. It interprets the date/time fields in the object fields using the given calendar. It performs the following steps when called:

1. Let isoDate be ? CalendarDateFromFields(calendar, fields, overflow).
2. Let time be ? RegulateTime(fields.[[Hour]], fields.[[Minute]], fields.[[Second]], fields.[[Millisecond]], fields.[[Microsecond]], fields.[[Nanosecond]], overflow).
3. Return CombineISODateAndTimeRecord(isoDate, time).

# 5.5.6 ToTemporalDateTime ( item [ , options ] )

The abstract operation ToTemporalDateTime takes argument item (an ECMAScript language value) and optional argument options (an ECMAScript language value) and returns either a normal completion containing a Temporal.PlainDateTime or a throw completion. Converts item to a new Temporal.PlainDateTime instance if possible, and throws otherwise. It performs the following steps when called:

1. If options is not present, set options to undefined.
2. If item is an Object, then
   1. If item has an [[InitializedTemporalDateTime]] internal slot, then
      1. Let resolvedOptions be ? GetOptionsObject(options).
      2. Perform ? GetTemporalOverflowOption(resolvedOptions).
      3. Return ! CreateTemporalDateTime(item.[[ISODateTime]], item.[[Calendar]]).
   2. If item has an [[InitializedTemporalZonedDateTime]] internal slot, then
      1. Let isoDateTime be GetISODateTimeFor(item.[[TimeZone]], item.[[EpochNanoseconds]]).
      2. Let resolvedOptions be ? GetOptionsObject(options).
      3. Perform ? GetTemporalOverflowOption(resolvedOptions).
      4. Return ! CreateTemporalDateTime(isoDateTime, item.[[Calendar]]).
   3. If item has an [[InitializedTemporalDate]] internal slot, then
      1. Let resolvedOptions be ? GetOptionsObject(options).
      2. Perform ? GetTemporalOverflowOption(resolvedOptions).
      3. Let isoDateTime be CombineISODateAndTimeRecord(item.[[ISODate]], MidnightTimeRecord()).
      4. Return ? CreateTemporalDateTime(isoDateTime, item.[[Calendar]]).
   4. Let calendar be ? GetTemporalCalendarIdentifierWithISODefault(item).
   5. Let fields be ? PrepareCalendarFields(calendar, item, « year, month, month-code, day », « hour, minute, second, millisecond, microsecond, nanosecond », «»).
   6. Let resolvedOptions be ? GetOptionsObject(options).
   7. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
   8. Let result be ? InterpretTemporalDateTimeFields(calendar, fields, overflow).
   9. Return ? CreateTemporalDateTime(result, calendar).
3. If item is not a String, throw a TypeError exception.
4. Let result be ? ParseISODateTime(item, « TemporalDateTimeString[~Zoned] »).
5. If result.[[Time]] is start-of-day, let time be MidnightTimeRecord(); else let time be result.[[Time]].
6. Let calendar be result.[[Calendar]].
7. If calendar is empty, set calendar to "iso8601".
8. Set calendar to ? CanonicalizeCalendar(calendar).
9. Let resolvedOptions be ? GetOptionsObject(options).
10. Perform ? GetTemporalOverflowOption(resolvedOptions).
11. Let isoDate be CreateISODateRecord(result.[[Year]], result.[[Month]], result.[[Day]]).
12. Let isoDateTime be CombineISODateAndTimeRecord(isoDate, time).
13. Return ? CreateTemporalDateTime(isoDateTime, calendar).

# 5.5.7 BalanceISODateTime ( year, month, day, hour, minute, second, millisecond, microsecond, nanosecond )

The abstract operation BalanceISODateTime takes arguments year (an integer), month (an integer), day (an integer), hour (an integer), minute (an integer), second (an integer), millisecond (an integer), microsecond (an integer), and nanosecond (an integer) and returns an ISO Date-Time Record. It balances the given date and time components so that each falls within its conventional range, carrying over excess into the next larger unit, and returns the result as an ISO Date-Time Record. It performs the following steps when called:

1. Let balancedTime be BalanceTime(hour, minute, second, millisecond, microsecond, nanosecond).
2. Let balancedDate be AddDaysToISODate(CreateISODateRecord(year, month, day), balancedTime.[[Days]]).
3. Return CombineISODateAndTimeRecord(balancedDate, balancedTime).

# 5.5.8 CreateTemporalDateTime ( isoDateTime, calendar [ , newTarget ] )

The abstract operation CreateTemporalDateTime takes arguments isoDateTime (an ISO Date-Time Record) and calendar (a calendar type) and optional argument newTarget (a constructor) and returns either a normal completion containing a Temporal.PlainDateTime or a throw completion. It creates a Temporal.PlainDateTime instance and fills the internal slots with valid values. It performs the following steps when called:

1. If ISODateTimeWithinLimits(isoDateTime) is false, throw a RangeError exception.
2. If newTarget is not present, set newTarget to %Temporal.PlainDateTime%.
3. Let object be ? OrdinaryCreateFromConstructor(newTarget, "%Temporal.PlainDateTime.prototype%", « [[InitializedTemporalDateTime]], [[ISODateTime]], [[Calendar]] »).
4. Set object.[[ISODateTime]] to isoDateTime.
5. Set object.[[Calendar]] to calendar.
6. Return object.

# 5.5.9 ISODateTimeToString ( isoDateTime, calendar, precision, showCalendar )

The abstract operation ISODateTimeToString takes arguments isoDateTime (an ISO Date-Time Record), calendar (a calendar type), precision (either an integer in the inclusive interval from 0 to 9, minute, or auto), and showCalendar (one of auto, always, never, or critical) and returns a String. It formats an ISO Date-Time Record into an ISO 8601 / RFC 9557 string, to the precision specified by precision. It performs the following steps when called:

1. Let yearString be PadISOYear(isoDateTime.[[ISODate]].[[Year]]).
2. Let monthString be ToZeroPaddedDecimalString(isoDateTime.[[ISODate]].[[Month]], 2).
3. Let dayString be ToZeroPaddedDecimalString(isoDateTime.[[ISODate]].[[Day]], 2).
4. Let subSecondNanoseconds be isoDateTime.[[Time]].[[Millisecond]] × 106 \+ isoDateTime.[[Time]].[[Microsecond]] × 103 \+ isoDateTime.[[Time]].[[Nanosecond]].
5. Let timeString be FormatTimeString(isoDateTime.[[Time]].[[Hour]], isoDateTime.[[Time]].[[Minute]], isoDateTime.[[Time]].[[Second]], subSecondNanoseconds, precision).
6. Let calendarString be FormatCalendarAnnotation(calendar, showCalendar).
7. Return the string-concatenation of yearString, the code unit 0x002D (HYPHEN-MINUS), monthString, the code unit 0x002D (HYPHEN-MINUS), dayString, 0x0054 (LATIN CAPITAL LETTER T), timeString, and calendarString.

# 5.5.10 CompareISODateTime ( isoDateTime1, isoDateTime2 )

The abstract operation CompareISODateTime takes arguments isoDateTime1 (an ISO Date-Time Record) and isoDateTime2 (an ISO Date-Time Record) and returns one of -1, 0, or 1. It performs a comparison of two date-times according to ISO 8601 calendar arithmetic. It performs the following steps when called:

1. Let dateResult be CompareISODate(isoDateTime1.[[ISODate]], isoDateTime2.[[ISODate]]).
2. If dateResult ≠ 0, return dateResult.
3. Return CompareTimeRecord(isoDateTime1.[[Time]], isoDateTime2.[[Time]]).

# 5.5.11 RoundISODateTime ( isoDateTime, increment, unit, roundingMode )

The abstract operation RoundISODateTime takes arguments isoDateTime (an ISO Date-Time Record), increment (a positive integer), unit (either a time unit or day), and roundingMode (a rounding mode) and returns an ISO Date-Time Record. It rounds the time part of a combined date and time, carrying over any excess into the date part. It performs the following steps when called:

1. Assert: ISODateTimeWithinLimits(isoDateTime) is true.
2. Let roundedTime be RoundTime(isoDateTime.[[Time]], increment, unit, roundingMode).
3. Let balanceResult be AddDaysToISODate(isoDateTime.[[ISODate]], roundedTime.[[Days]]).
4. Return CombineISODateAndTimeRecord(balanceResult, roundedTime).

# 5.5.12 DifferenceISODateTime ( isoDateTime1, isoDateTime2, calendar, largestUnit )

The abstract operation DifferenceISODateTime takes arguments isoDateTime1 (an ISO Date-Time Record), isoDateTime2 (an ISO Date-Time Record), calendar (a calendar type), and largestUnit (a Temporal unit) and returns an Internal Duration Record. The returned Internal Duration Record contains the elapsed duration from a first date and time, until a second date and time, according to the reckoning of the given calendar. The given date and time units are all in the ISO 8601 calendar. It performs the following steps when called:

1. Assert: ISODateTimeWithinLimits(isoDateTime1) is true.
2. Assert: ISODateTimeWithinLimits(isoDateTime2) is true.
3. Let timeDuration be DifferenceTime(isoDateTime1.[[Time]], isoDateTime2.[[Time]]).
4. Let timeSign be TimeDurationSign(timeDuration).
5. Let dateSign be CompareISODate(isoDateTime1.[[ISODate]], isoDateTime2.[[ISODate]]).
6. Let adjustedDate be isoDateTime2.[[ISODate]].
7. If timeSign = dateSign, then
   1. Set adjustedDate to AddDaysToISODate(adjustedDate, timeSign).
   2. Set timeDuration to ! Add24HourDaysToTimeDuration(timeDuration, -timeSign).
8. Let dateLargestUnit be LargerOfTwoTemporalUnits(day, largestUnit).
9. Let dateDifference be CalendarDateUntil(calendar, isoDateTime1.[[ISODate]], adjustedDate, dateLargestUnit).
10. If largestUnit is not dateLargestUnit, then
    1. Set timeDuration to ! Add24HourDaysToTimeDuration(timeDuration, dateDifference.[[Days]]).
    2. Set dateDifference.[[Days]] to 0.
11. Return CombineDateAndTimeDuration(dateDifference, timeDuration).

# 5.5.13 DifferencePlainDateTimeWithRounding ( isoDateTime1, isoDateTime2, calendar, largestUnit, roundingIncrement, smallestUnit, roundingMode )

The abstract operation DifferencePlainDateTimeWithRounding takes arguments isoDateTime1 (an ISO Date-Time Record), isoDateTime2 (an ISO Date-Time Record), calendar (a calendar type), largestUnit (a Temporal unit), roundingIncrement (a positive integer), smallestUnit (a Temporal unit), and roundingMode (a rounding mode) and returns either a normal completion containing an Internal Duration Record or a throw completion. It computes the difference between isoDateTime1 and isoDateTime2, rounding the result according to roundingIncrement, smallestUnit, and roundingMode. It throws a RangeError if either datetime is outside the representable limits. It performs the following steps when called:

1. If CompareISODateTime(isoDateTime1, isoDateTime2) = 0, return CombineDateAndTimeDuration(ZeroDateDuration(), 0).
2. If ISODateTimeWithinLimits(isoDateTime1) is false or ISODateTimeWithinLimits(isoDateTime2) is false, throw a RangeError exception.
3. Let diff be DifferenceISODateTime(isoDateTime1, isoDateTime2, calendar, largestUnit).
4. If smallestUnit is nanosecond and roundingIncrement = 1, return diff.
5. Let originEpochNs be GetUTCEpochNanoseconds(isoDateTime1).
6. Let destEpochNs be GetUTCEpochNanoseconds(isoDateTime2).
7. Return ? RoundRelativeDuration(diff, originEpochNs, destEpochNs, isoDateTime1, unset, calendar, largestUnit, roundingIncrement, smallestUnit, roundingMode).

# 5.5.14 DifferencePlainDateTimeWithTotal ( isoDateTime1, isoDateTime2, calendar, unit )

The abstract operation DifferencePlainDateTimeWithTotal takes arguments isoDateTime1 (an ISO Date-Time Record), isoDateTime2 (an ISO Date-Time Record), calendar (a calendar type), and unit (a Temporal unit) and returns either a normal completion containing a mathematical value or a throw completion. It computes the total difference between isoDateTime1 and isoDateTime2 as a mathematical value in units of unit. It throws a RangeError if either datetime is outside the representable limits. It performs the following steps when called:

1. If CompareISODateTime(isoDateTime1, isoDateTime2) = 0, return 0.
2. If ISODateTimeWithinLimits(isoDateTime1) is false or ISODateTimeWithinLimits(isoDateTime2) is false, throw a RangeError exception.
3. Let diff be DifferenceISODateTime(isoDateTime1, isoDateTime2, calendar, unit).
4. If unit is nanosecond, return diff.[[Time]].
5. Let originEpochNs be GetUTCEpochNanoseconds(isoDateTime1).
6. Let destEpochNs be GetUTCEpochNanoseconds(isoDateTime2).
7. Return ? TotalRelativeDuration(diff, originEpochNs, destEpochNs, isoDateTime1, unset, calendar, unit).

# 5.5.15 DifferenceTemporalPlainDateTime ( operation, dateTime, other, options )

The abstract operation DifferenceTemporalPlainDateTime takes arguments operation (either since or until), dateTime (a Temporal.PlainDateTime), other (an ECMAScript language value), and options (an ECMAScript language value) and returns either a normal completion containing a Temporal.Duration or a throw completion. It computes the difference between the two times represented by dateTime and other, optionally rounds it, and returns it as a Temporal.Duration object. It performs the following steps when called:

1. Set other to ? ToTemporalDateTime(other).
2. If CalendarEquals(dateTime.[[Calendar]], other.[[Calendar]]) is false, throw a RangeError exception.
3. Let resolvedOptions be ? GetOptionsObject(options).
4. Let settings be ? GetDifferenceSettings(operation, resolvedOptions, datetime, « », nanosecond, day).
5. If CompareISODateTime(dateTime.[[ISODateTime]], other.[[ISODateTime]]) = 0, return ! CreateTemporalDuration(0, 0, 0, 0, 0, 0, 0, 0, 0, 0).
6. Let internalDuration be ? DifferencePlainDateTimeWithRounding(dateTime.[[ISODateTime]], other.[[ISODateTime]], dateTime.[[Calendar]], settings.[[LargestUnit]], settings.[[RoundingIncrement]], settings.[[SmallestUnit]], settings.[[RoundingMode]]).
7. Let result be ! TemporalDurationFromInternal(internalDuration, settings.[[LargestUnit]]).
8. If operation is since, set result to CreateNegatedTemporalDuration(result).
9. Return result.

# 5.5.16 AddDurationToDateTime ( operation, dateTime, temporalDurationLike, options )

The abstract operation AddDurationToDateTime takes arguments operation (either add or subtract), dateTime (a Temporal.PlainDateTime), temporalDurationLike (an ECMAScript language value), and options (an ECMAScript language value) and returns either a normal completion containing a Temporal.PlainDateTime or a throw completion. It adds/subtracts temporalDurationLike to/from dateTime, returning a point in time that is in the future/past relative to datetime. It performs the following steps when called:

1. Let duration be ? ToTemporalDuration(temporalDurationLike).
2. If operation is subtract, set duration to CreateNegatedTemporalDuration(duration).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
5. Let internalDuration be ToInternalDurationRecordWith24HourDays(duration).
6. Let timeResult be AddTime(dateTime.[[ISODateTime]].[[Time]], internalDuration.[[Time]]).
7. Let dateDuration be ? AdjustDateDurationRecord(internalDuration.[[Date]], timeResult.[[Days]]).
8. Let addedDate be ? CalendarDateAdd(dateTime.[[Calendar]], dateTime.[[ISODateTime]].[[ISODate]], dateDuration, overflow).
9. Let result be CombineISODateAndTimeRecord(addedDate, timeResult).
10. Return ? CreateTemporalDateTime(result, dateTime.[[Calendar]]).

# 6 Temporal.ZonedDateTime Objects

A Temporal.ZonedDateTime object is an Object referencing a fixed point in time with nanoseconds precision, and containing Object values corresponding to a particular time zone and calendar system.

# 6.1 The Temporal.ZonedDateTime Constructor

The Temporal.ZonedDateTime constructor:

- creates and initializes a new Temporal.ZonedDateTime object when called as a constructor.
- is not intended to be called as a function and will throw an exception when called in that manner.
- may be used as the value of an `extends` clause of a class definition. Subclass constructors that intend to inherit the specified Temporal.ZonedDateTime behaviour must include a super call to the %Temporal.ZonedDateTime% constructor to create and initialize subclass instances with the necessary internal slots.

# 6.1.1 Temporal.ZonedDateTime ( epochNanoseconds, timeZone [ , calendar ] )

This function performs the following steps when called:

1. If NewTarget is undefined, throw a TypeError exception.
2. Set epochNanoseconds to ? ToBigInt(epochNanoseconds).
3. If IsValidEpochNanoseconds(epochNanoseconds) is false, throw a RangeError exception.
4. If timeZone is not a String, throw a TypeError exception.
5. Let timeZoneParse be ? ParseTimeZoneIdentifier(timeZone).
6. If timeZoneParse.[[OffsetMinutes]] is empty, then
   1. Let identifierRecord be GetAvailableNamedTimeZoneIdentifier(timeZoneParse.[[Name]]).
   2. If identifierRecord is empty, throw a RangeError exception.
   3. Set timeZone to identifierRecord.[[Identifier]].
7. Else,
   1. Set timeZone to FormatOffsetTimeZoneIdentifier(timeZoneParse.[[OffsetMinutes]]).
8. If calendar is undefined, set calendar to "iso8601".
9. If calendar is not a String, throw a TypeError exception.
10. Set calendar to ? CanonicalizeCalendar(calendar).
11. Return ? CreateTemporalZonedDateTime(epochNanoseconds, timeZone, calendar, NewTarget).

# 6.2 Properties of the Temporal.ZonedDateTime Constructor

The value of the [[Prototype]] internal slot of the Temporal.ZonedDateTime constructor is the intrinsic object %Function.prototype%.

The Temporal.ZonedDateTime constructor has the following properties:

# 6.2.1 Temporal.ZonedDateTime.prototype

The initial value of `Temporal.ZonedDateTime.prototype` is %Temporal.ZonedDateTime.prototype%.

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: false }.

# 6.2.2 Temporal.ZonedDateTime.from ( item [ , options ] )

This function performs the following steps when called:

1. Return ? ToTemporalZonedDateTime(item, options).

# 6.2.3 Temporal.ZonedDateTime.compare ( one, two )

This function performs the following steps when called:

1. Set one to ? ToTemporalZonedDateTime(one).
2. Set two to ? ToTemporalZonedDateTime(two).
3. Return 𝔽(CompareEpochNanoseconds(one.[[EpochNanoseconds]], two.[[EpochNanoseconds]])).

# 6.3 Properties of the Temporal.ZonedDateTime Prototype Object

The Temporal.ZonedDateTime prototype object

- is itself an ordinary object.
- is not a Temporal.ZonedDateTime instance and does not have a [[InitializedTemporalZonedDateTime]] internal slot.
- has a [[Prototype]] internal slot whose value is %Object.prototype%.

Note

An ECMAScript implementation that includes the ECMA-402 Internationalization API extends this prototype with additional properties in order to represent calendar data.

# 6.3.1 Temporal.ZonedDateTime.prototype.constructor

The initial value of `Temporal.ZonedDateTime.prototype.constructor` is %Temporal.ZonedDateTime%.

# 6.3.2 Temporal.ZonedDateTime.prototype[ %Symbol.toStringTag% ]

The initial value of the %Symbol.toStringTag% property is the String "Temporal.ZonedDateTime".

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }.

# 6.3.3 get Temporal.ZonedDateTime.prototype.calendarId

`Temporal.ZonedDateTime.prototype.calendarId` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return zonedDateTime.[[Calendar]].

# 6.3.4 get Temporal.ZonedDateTime.prototype.timeZoneId

`Temporal.ZonedDateTime.prototype.timeZoneId` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return zonedDateTime.[[TimeZone]].

# 6.3.5 get Temporal.ZonedDateTime.prototype.era

`Temporal.ZonedDateTime.prototype.era` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[Era]].

# 6.3.6 get Temporal.ZonedDateTime.prototype.eraYear

`Temporal.ZonedDateTime.prototype.eraYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Let result be CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[EraYear]].
5. If result is undefined, return undefined.
6. Return 𝔽(result).

# 6.3.7 get Temporal.ZonedDateTime.prototype.year

`Temporal.ZonedDateTime.prototype.year` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[Year]]).

# 6.3.8 get Temporal.ZonedDateTime.prototype.month

`Temporal.ZonedDateTime.prototype.month` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[Month]]).

# 6.3.9 get Temporal.ZonedDateTime.prototype.monthCode

`Temporal.ZonedDateTime.prototype.monthCode` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[MonthCode]].

# 6.3.10 get Temporal.ZonedDateTime.prototype.day

`Temporal.ZonedDateTime.prototype.day` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[Day]]).

# 6.3.11 get Temporal.ZonedDateTime.prototype.hour

`Temporal.ZonedDateTime.prototype.hour` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(isoDateTime.[[Time]].[[Hour]]).

# 6.3.12 get Temporal.ZonedDateTime.prototype.minute

`Temporal.ZonedDateTime.prototype.minute` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(isoDateTime.[[Time]].[[Minute]]).

# 6.3.13 get Temporal.ZonedDateTime.prototype.second

`Temporal.ZonedDateTime.prototype.second` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(isoDateTime.[[Time]].[[Second]]).

# 6.3.14 get Temporal.ZonedDateTime.prototype.millisecond

`Temporal.ZonedDateTime.prototype.millisecond` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(isoDateTime.[[Time]].[[Millisecond]]).

# 6.3.15 get Temporal.ZonedDateTime.prototype.microsecond

`Temporal.ZonedDateTime.prototype.microsecond` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(isoDateTime.[[Time]].[[Microsecond]]).

# 6.3.16 get Temporal.ZonedDateTime.prototype.nanosecond

`Temporal.ZonedDateTime.prototype.nanosecond` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(isoDateTime.[[Time]].[[Nanosecond]]).

# 6.3.17 get Temporal.ZonedDateTime.prototype.epochMilliseconds

`Temporal.ZonedDateTime.prototype.epochMilliseconds` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let ns be zonedDateTime.[[EpochNanoseconds]].
4. Let ms be floor(ℝ(ns) / 106).
5. Return 𝔽(ms).

# 6.3.18 get Temporal.ZonedDateTime.prototype.epochNanoseconds

`Temporal.ZonedDateTime.prototype.epochNanoseconds` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return zonedDateTime.[[EpochNanoseconds]].

# 6.3.19 get Temporal.ZonedDateTime.prototype.dayOfWeek

`Temporal.ZonedDateTime.prototype.dayOfWeek` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[DayOfWeek]]).

# 6.3.20 get Temporal.ZonedDateTime.prototype.dayOfYear

`Temporal.ZonedDateTime.prototype.dayOfYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[DayOfYear]]).

# 6.3.21 get Temporal.ZonedDateTime.prototype.weekOfYear

`Temporal.ZonedDateTime.prototype.weekOfYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Let result be CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[WeekOfYear]].[[Week]].
5. If result is undefined, return undefined.
6. Return 𝔽(result).

# 6.3.22 get Temporal.ZonedDateTime.prototype.yearOfWeek

`Temporal.ZonedDateTime.prototype.yearOfWeek` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Let result be CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[WeekOfYear]].[[Year]].
5. If result is undefined, return undefined.
6. Return 𝔽(result).

# 6.3.23 get Temporal.ZonedDateTime.prototype.hoursInDay

`Temporal.ZonedDateTime.prototype.hoursInDay` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let timeZone be zonedDateTime.[[TimeZone]].
4. Let isoDateTime be GetISODateTimeFor(timeZone, zonedDateTime.[[EpochNanoseconds]]).
5. Let today be isoDateTime.[[ISODate]].
6. Let tomorrow be AddDaysToISODate(today, 1).
7. Let todayNs be ? GetStartOfDay(timeZone, today).
8. Let tomorrowNs be ? GetStartOfDay(timeZone, tomorrow).
9. Let diff be TimeDurationFromEpochNanosecondsDifference(tomorrowNs, todayNs).
10. Return 𝔽(TotalTimeDuration(diff, hour)).

# 6.3.24 get Temporal.ZonedDateTime.prototype.daysInWeek

`Temporal.ZonedDateTime.prototype.daysInWeek` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[DaysInWeek]]).

# 6.3.25 get Temporal.ZonedDateTime.prototype.daysInMonth

`Temporal.ZonedDateTime.prototype.daysInMonth` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[DaysInMonth]]).

# 6.3.26 get Temporal.ZonedDateTime.prototype.daysInYear

`Temporal.ZonedDateTime.prototype.daysInYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[DaysInYear]]).

# 6.3.27 get Temporal.ZonedDateTime.prototype.monthsInYear

`Temporal.ZonedDateTime.prototype.monthsInYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return 𝔽(CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[MonthsInYear]]).

# 6.3.28 get Temporal.ZonedDateTime.prototype.inLeapYear

`Temporal.ZonedDateTime.prototype.inLeapYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return CalendarISOToDate(zonedDateTime.[[Calendar]], isoDateTime.[[ISODate]]).[[InLeapYear]].

# 6.3.29 get Temporal.ZonedDateTime.prototype.offsetNanoseconds

`Temporal.ZonedDateTime.prototype.offsetNanoseconds` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return 𝔽(GetOffsetNanosecondsFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]])).

# 6.3.30 get Temporal.ZonedDateTime.prototype.offset

`Temporal.ZonedDateTime.prototype.offset` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let offsetNanoseconds be GetOffsetNanosecondsFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return FormatUTCOffsetNanoseconds(offsetNanoseconds).

# 6.3.31 Temporal.ZonedDateTime.prototype.with ( temporalZonedDateTimeLike [ , options ] )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. If ? IsPartialTemporalObject(temporalZonedDateTimeLike) is false, throw a TypeError exception.
4. Let epochNs be zonedDateTime.[[EpochNanoseconds]].
5. Let timeZone be zonedDateTime.[[TimeZone]].
6. Let calendar be zonedDateTime.[[Calendar]].
7. Let offsetNanoseconds be GetOffsetNanosecondsFor(timeZone, epochNs).
8. Let isoDateTime be GetISODateTimeFor(timeZone, epochNs).
9. Let fields be ISODateToFields(calendar, isoDateTime.[[ISODate]], date).
10. Set fields.[[Hour]] to isoDateTime.[[Time]].[[Hour]].
11. Set fields.[[Minute]] to isoDateTime.[[Time]].[[Minute]].
12. Set fields.[[Second]] to isoDateTime.[[Time]].[[Second]].
13. Set fields.[[Millisecond]] to isoDateTime.[[Time]].[[Millisecond]].
14. Set fields.[[Microsecond]] to isoDateTime.[[Time]].[[Microsecond]].
15. Set fields.[[Nanosecond]] to isoDateTime.[[Time]].[[Nanosecond]].
16. Set fields.[[OffsetString]] to FormatUTCOffsetNanoseconds(offsetNanoseconds).
17. Let partialZonedDateTime be ? PrepareCalendarFields(calendar, temporalZonedDateTimeLike, « year, month, month-code, day », « hour, minute, second, millisecond, microsecond, nanosecond, offset », partial).
18. Set fields to CalendarMergeFields(calendar, fields, partialZonedDateTime).
19. Let resolvedOptions be ? GetOptionsObject(options).
20. Let disambiguation be ? GetTemporalDisambiguationOption(resolvedOptions).
21. Let offset be ? GetTemporalOffsetOption(resolvedOptions, prefer).
22. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
23. Let dateTimeResult be ? InterpretTemporalDateTimeFields(calendar, fields, overflow).
24. Let newOffsetNanoseconds be ! ParseDateTimeUTCOffset(fields.[[OffsetString]]).
25. Let epochNanoseconds be ? InterpretISODateTimeOffset(dateTimeResult.[[ISODate]], dateTimeResult.[[Time]], option, newOffsetNanoseconds, timeZone, disambiguation, offset, match-exactly).
26. Return ! CreateTemporalZonedDateTime(epochNanoseconds, timeZone, calendar).

# 6.3.32 Temporal.ZonedDateTime.prototype.withPlainTime ( [ plainTimeLike ] )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let timeZone be zonedDateTime.[[TimeZone]].
4. Let calendar be zonedDateTime.[[Calendar]].
5. Let isoDateTime be GetISODateTimeFor(timeZone, zonedDateTime.[[EpochNanoseconds]]).
6. If plainTimeLike is undefined, then
   1. Let epochNs be ? GetStartOfDay(timeZone, isoDateTime.[[ISODate]]).
7. Else,
   1. Let plainTime be ? ToTemporalTime(plainTimeLike).
   2. Let resultISODateTime be CombineISODateAndTimeRecord(isoDateTime.[[ISODate]], plainTime.[[Time]]).
   3. Let epochNs be ? GetEpochNanosecondsFor(timeZone, resultISODateTime, compatible).
8. Return ! CreateTemporalZonedDateTime(epochNs, timeZone, calendar).

# 6.3.33 Temporal.ZonedDateTime.prototype.withTimeZone ( timeZoneLike )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let timeZone be ? ToTemporalTimeZoneIdentifier(timeZoneLike).
4. Return ! CreateTemporalZonedDateTime(zonedDateTime.[[EpochNanoseconds]], timeZone, zonedDateTime.[[Calendar]]).

# 6.3.34 Temporal.ZonedDateTime.prototype.withCalendar ( calendarLike )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let calendar be ? ToTemporalCalendarIdentifier(calendarLike).
4. Return ! CreateTemporalZonedDateTime(zonedDateTime.[[EpochNanoseconds]], zonedDateTime.[[TimeZone]], calendar).

# 6.3.35 Temporal.ZonedDateTime.prototype.add ( temporalDurationLike [ , options ] )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return ? AddDurationToZonedDateTime(add, zonedDateTime, temporalDurationLike, options).

# 6.3.36 Temporal.ZonedDateTime.prototype.subtract ( temporalDurationLike [ , options ] )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return ? AddDurationToZonedDateTime(subtract, zonedDateTime, temporalDurationLike, options).

# 6.3.37 Temporal.ZonedDateTime.prototype.until ( other [ , options ] )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return ? DifferenceTemporalZonedDateTime(until, zonedDateTime, other, options).

# 6.3.38 Temporal.ZonedDateTime.prototype.since ( other [ , options ] )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return ? DifferenceTemporalZonedDateTime(since, zonedDateTime, other, options).

# 6.3.39 Temporal.ZonedDateTime.prototype.round ( roundTo )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. If roundTo is undefined, throw a TypeError exception.
4. If roundTo is a String, then
   1. Let paramString be roundTo.
   2. Set roundTo to OrdinaryObjectCreate(null).
   3. Perform ! CreateDataPropertyOrThrow(roundTo, "smallestUnit", paramString).
5. Else,
   1. Set roundTo to ? GetOptionsObject(roundTo).
6. NOTE: The following steps read options and perform independent validation in alphabetical order (GetRoundingIncrementOption reads "roundingIncrement" and GetRoundingModeOption reads "roundingMode").
7. Let roundingIncrement be ? GetRoundingIncrementOption(roundTo).
8. Let roundingMode be ? GetRoundingModeOption(roundTo, half-expand).
9. Let smallestUnit be ? GetTemporalUnitValuedOption(roundTo, "smallestUnit", required).
10. Perform ? ValidateTemporalUnitValue(smallestUnit, time, « day »).
11. If smallestUnit is day, then
    1. Let maximum be 1.
    2. Let inclusive be true.
12. Else,
    1. Let maximum be MaximumTemporalDurationRoundingIncrement(smallestUnit).
    2. Assert: maximum is not unset.
    3. Let inclusive be false.
13. Perform ? ValidateTemporalRoundingIncrement(roundingIncrement, maximum, inclusive).
14. If smallestUnit is nanosecond and roundingIncrement = 1, then
    1. Return ! CreateTemporalZonedDateTime(zonedDateTime.[[EpochNanoseconds]], zonedDateTime.[[TimeZone]], zonedDateTime.[[Calendar]]).
15. Let thisNs be zonedDateTime.[[EpochNanoseconds]].
16. Let timeZone be zonedDateTime.[[TimeZone]].
17. Let calendar be zonedDateTime.[[Calendar]].
18. Let isoDateTime be GetISODateTimeFor(timeZone, thisNs).
19. If smallestUnit is day, then
    1. Let dateStart be isoDateTime.[[ISODate]].
    2. Let dateEnd be AddDaysToISODate(dateStart, 1).
    3. Let startNs be ? GetStartOfDay(timeZone, dateStart).
    4. Assert: thisNs ≥ startNs.
    5. Let endNs be ? GetStartOfDay(timeZone, dateEnd).
    6. Assert: thisNs < endNs.
    7. Let dayLengthNs be ℝ(endNs \- startNs).
    8. Let dayProgressNs be TimeDurationFromEpochNanosecondsDifference(thisNs, startNs).
    9. Let roundedDayNs be ! RoundTimeDurationToIncrement(dayProgressNs, dayLengthNs, roundingMode).
    10. Let epochNanoseconds be AddTimeDurationToEpochNanoseconds(roundedDayNs, startNs).
20. Else,
    1. Let roundResult be RoundISODateTime(isoDateTime, roundingIncrement, smallestUnit, roundingMode).
    2. Let offsetNanoseconds be GetOffsetNanosecondsFor(timeZone, thisNs).
    3. Let epochNanoseconds be ? InterpretISODateTimeOffset(roundResult.[[ISODate]], roundResult.[[Time]], option, offsetNanoseconds, timeZone, compatible, prefer, match-exactly).
21. Return ! CreateTemporalZonedDateTime(epochNanoseconds, timeZone, calendar).

# 6.3.40 Temporal.ZonedDateTime.prototype.equals ( other )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Set other to ? ToTemporalZonedDateTime(other).
4. If zonedDateTime.[[EpochNanoseconds]] ≠ other.[[EpochNanoseconds]], return false.
5. If TimeZoneEquals(zonedDateTime.[[TimeZone]], other.[[TimeZone]]) is false, return false.
6. Return CalendarEquals(zonedDateTime.[[Calendar]], other.[[Calendar]]).

# 6.3.41 Temporal.ZonedDateTime.prototype.toString ( [ options ] )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. NOTE: The following steps read options and perform independent validation in alphabetical order (GetTemporalShowCalendarNameOption reads "calendarName", GetTemporalFractionalSecondDigitsOption reads "fractionalSecondDigits", GetTemporalShowOffsetOption reads "offset", and GetRoundingModeOption reads "roundingMode").
5. Let showCalendar be ? GetTemporalShowCalendarNameOption(resolvedOptions).
6. Let digits be ? GetTemporalFractionalSecondDigitsOption(resolvedOptions).
7. Let showOffset be ? GetTemporalShowOffsetOption(resolvedOptions).
8. Let roundingMode be ? GetRoundingModeOption(resolvedOptions, trunc).
9. Let smallestUnit be ? GetTemporalUnitValuedOption(resolvedOptions, "smallestUnit", unset).
10. Let showTimeZone be ? GetTemporalShowTimeZoneNameOption(resolvedOptions).
11. Perform ? ValidateTemporalUnitValue(smallestUnit, time).
12. If smallestUnit is hour, throw a RangeError exception.
13. Let precision be ToSecondsStringPrecisionRecord(smallestUnit, digits).
14. Return TemporalZonedDateTimeToString(zonedDateTime, precision.[[Precision]], showCalendar, showTimeZone, showOffset, precision.[[Increment]], precision.[[Unit]], roundingMode).

# 6.3.42 Temporal.ZonedDateTime.prototype.toLocaleString ( [ locales [ , options ] ] )

An ECMAScript implementation that includes the ECMA-402 Internationalization API must implement this method as specified in the ECMA-402 specification. If an ECMAScript implementation does not include the ECMA-402 API the following specification of this method is used.

The meanings of the optional parameters to this method are defined in the ECMA-402 specification; implementations that do not include ECMA-402 support must not use those parameter positions for anything else.

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return TemporalZonedDateTimeToString(zonedDateTime, auto, auto, auto, auto).

# 6.3.43 Temporal.ZonedDateTime.prototype.toJSON ( )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return TemporalZonedDateTimeToString(zonedDateTime, auto, auto, auto, auto).

# 6.3.44 Temporal.ZonedDateTime.prototype.valueOf ( )

This method performs the following steps when called:

1. Throw a TypeError exception.

Note

This method always throws, because in the absence of `valueOf()`, expressions with arithmetic operators such as `zonedDateTime1 > zonedDateTime2` would fall back to being equivalent to `zonedDateTime1.toString() > zonedDateTime2.toString()`. Lexicographical comparison of serialized strings might not seem obviously wrong, because the result would sometimes be correct. Implementations are encouraged to phrase the error message to point users to `Temporal.ZonedDateTime.compare()`, `Temporal.ZonedDateTime.prototype.equals()`, and/or `Temporal.ZonedDateTime.prototype.toString()`.

# 6.3.45 Temporal.ZonedDateTime.prototype.startOfDay ( )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let timeZone be zonedDateTime.[[TimeZone]].
4. Let calendar be zonedDateTime.[[Calendar]].
5. Let isoDateTime be GetISODateTimeFor(timeZone, zonedDateTime.[[EpochNanoseconds]]).
6. Let epochNanoseconds be ? GetStartOfDay(timeZone, isoDateTime.[[ISODate]]).
7. Return ! CreateTemporalZonedDateTime(epochNanoseconds, timeZone, calendar).

# 6.3.46 Temporal.ZonedDateTime.prototype.getTimeZoneTransition ( directionParam )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let timeZone be zonedDateTime.[[TimeZone]].
4. If directionParam is undefined, throw a TypeError exception.
5. If directionParam is a String, then
   1. Let paramString be directionParam.
   2. Set directionParam to OrdinaryObjectCreate(null).
   3. Perform ! CreateDataPropertyOrThrow(directionParam, "direction", paramString).
6. Else,
   1. Set directionParam to ? GetOptionsObject(directionParam).
7. Let direction be ? GetDirectionOption(directionParam).
8. If IsOffsetTimeZoneIdentifier(timeZone) is true, return null.
9. If direction is next, then
   1. Let transition be GetNamedTimeZoneNextTransition(timeZone, zonedDateTime.[[EpochNanoseconds]]).
10. Else,
    1. Assert: direction is previous.
    2. Let transition be GetNamedTimeZonePreviousTransition(timeZone, zonedDateTime.[[EpochNanoseconds]]).
11. If transition is null, return null.
12. Return ! CreateTemporalZonedDateTime(transition, timeZone, zonedDateTime.[[Calendar]]).

# 6.3.47 Temporal.ZonedDateTime.prototype.toInstant ( )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Return ! CreateTemporalInstant(zonedDateTime.[[EpochNanoseconds]]).

# 6.3.48 Temporal.ZonedDateTime.prototype.toPlainDate ( )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return ! CreateTemporalDate(isoDateTime.[[ISODate]], zonedDateTime.[[Calendar]]).

# 6.3.49 Temporal.ZonedDateTime.prototype.toPlainTime ( )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return ! CreateTemporalTime(isoDateTime.[[Time]]).

# 6.3.50 Temporal.ZonedDateTime.prototype.toPlainDateTime ( )

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let isoDateTime be GetISODateTimeFor(zonedDateTime.[[TimeZone]], zonedDateTime.[[EpochNanoseconds]]).
4. Return ! CreateTemporalDateTime(isoDateTime, zonedDateTime.[[Calendar]]).

# 6.4 Properties of Temporal.ZonedDateTime Instances

Temporal.ZonedDateTime instances are ordinary objects that inherit properties from the %Temporal.ZonedDateTime.prototype% intrinsic object. Temporal.ZonedDateTime instances are initially created with the internal slots described in Table 8.

| Table 8: Internal Slots of Temporal.ZonedDateTime Instances Internal Slot | Description                                                                                                    |
| ------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| [[InitializedTemporalZonedDateTime]]                                      | The only specified use of this slot is for distinguishing Temporal.ZonedDateTime instances from other objects. |
| [[EpochNanoseconds]]                                                      | A BigInt representing the number of nanoseconds since the epoch.                                               |
| [[TimeZone]]                                                              | An available time zone identifier.                                                                             |
| [[Calendar]]                                                              | A calendar type.                                                                                               |

# 6.5 Abstract Operations

# 6.5.1 InterpretISODateTimeOffset ( isoDate, time, offsetBehaviour, offsetNanoseconds, timeZone, disambiguation, offsetOption, matchBehaviour )

The abstract operation InterpretISODateTimeOffset takes arguments isoDate (an ISO Date Record), time (either a Time Record or start-of-day), offsetBehaviour (one of option, exact, or wall), offsetNanoseconds (an integer in the interval from -86400 × 109 (exclusive) to 86400 × 109 (exclusive)), timeZone (an available time zone identifier), disambiguation (one of earlier, later, compatible, or reject), offsetOption (one of ignore, use, prefer, or reject), and matchBehaviour (either match-exactly or match-minutes) and returns either a normal completion containing a BigInt or a throw completion.

It determines the exact time in timeZone corresponding to the given calendar date and time, and the given UTC offset in nanoseconds. In the case of more than one possible exact time, or no possible exact time, an answer is determined using offsetBehaviour, disambiguation and offsetOption.

As a special case when parsing ISO 8601 / RFC 9557 strings which are only required to specify time zone offsets to minutes precision, if matchBehaviour is match minutes, then a value for offsetNanoseconds that is rounded to the nearest minute will be accepted in those cases where offsetNanoseconds is compared against timeZone's offset. If matchBehaviour is match exactly, then this does not happen.

It performs the following steps when called:

1. If time is start-of-day, then
   1. Assert: offsetBehaviour is wall.
   2. Assert: offsetNanoseconds = 0.
   3. Return ? GetStartOfDay(timeZone, isoDate).
2. Let isoDateTime be CombineISODateAndTimeRecord(isoDate, time).
3. If offsetBehaviour is wall, or offsetBehaviour is option and offsetOption is ignore, then
   1. Return ? GetEpochNanosecondsFor(timeZone, isoDateTime, disambiguation).
4. If offsetBehaviour is exact, or offsetBehaviour is option and offsetOption is use, then
   1. Let balanced be BalanceISODateTime(isoDate.[[Year]], isoDate.[[Month]], isoDate.[[Day]], time.[[Hour]], time.[[Minute]], time.[[Second]], time.[[Millisecond]], time.[[Microsecond]], time.[[Nanosecond]] \- offsetNanoseconds).
   2. Perform ? CheckISODaysRange(balanced.[[ISODate]]).
   3. Let epochNanoseconds be GetUTCEpochNanoseconds(balanced).
   4. If IsValidEpochNanoseconds(epochNanoseconds) is false, throw a RangeError exception.
   5. Return epochNanoseconds.
5. Assert: offsetBehaviour is option.
6. Assert: offsetOption is prefer or reject.
7. Perform ? CheckISODaysRange(isoDate).
8. Let utcEpochNanoseconds be GetUTCEpochNanoseconds(isoDateTime).
9. Let possibleEpochNs be ? GetPossibleEpochNanoseconds(timeZone, isoDateTime).
10. For each element candidate of possibleEpochNs, do
    1. Let candidateOffset be utcEpochNanoseconds \- candidate.
    2. If candidateOffset = offsetNanoseconds, return candidate.
    3. If matchBehaviour is match-minutes, then
    4. Let roundedCandidateNanoseconds be RoundNumberToIncrement(candidateOffset, 60 × 109, half-expand).
    5. If roundedCandidateNanoseconds = offsetNanoseconds, return candidate.
11. If offsetOption is reject, throw a RangeError exception.
12. Return ? DisambiguatePossibleEpochNanoseconds(possibleEpochNs, timeZone, isoDateTime, disambiguation).

# 6.5.2 ToTemporalZonedDateTime ( item [ , options ] )

The abstract operation ToTemporalZonedDateTime takes argument item (an ECMAScript language value) and optional argument options (an ECMAScript language value) and returns either a normal completion containing a Temporal.ZonedDateTime, or a throw completion. Converts item to a new Temporal.ZonedDateTime instance if possible, and throws otherwise. It performs the following steps when called:

1. If options is not present, set options to undefined.
2. Let hasUTCDesignator be false.
3. Let matchBehaviour be match-exactly.
4. If item is an Object, then
   1. If item has an [[InitializedTemporalZonedDateTime]] internal slot, then
      1. NOTE: The following steps, and similar ones below, read options and perform independent validation in alphabetical order (GetTemporalDisambiguationOption reads "disambiguation", GetTemporalOffsetOption reads "offset", and GetTemporalOverflowOption reads "overflow").
      2. Let resolvedOptions be ? GetOptionsObject(options).
      3. Perform ? GetTemporalDisambiguationOption(resolvedOptions).
      4. Perform ? GetTemporalOffsetOption(resolvedOptions, reject).
      5. Perform ? GetTemporalOverflowOption(resolvedOptions).
      6. Return ! CreateTemporalZonedDateTime(item.[[EpochNanoseconds]], item.[[TimeZone]], item.[[Calendar]]).
   2. Let calendar be ? GetTemporalCalendarIdentifierWithISODefault(item).
   3. Let fields be ? PrepareCalendarFields(calendar, item, « year, month, month-code, day », « hour, minute, second, millisecond, microsecond, nanosecond, offset, time-zone », « time-zone »).
   4. Let timeZone be fields.[[TimeZone]].
   5. Let offsetString be fields.[[OffsetString]].
   6. Let resolvedOptions be ? GetOptionsObject(options).
   7. Let disambiguation be ? GetTemporalDisambiguationOption(resolvedOptions).
   8. Let offsetOption be ? GetTemporalOffsetOption(resolvedOptions, reject).
   9. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
   10. Let result be ? InterpretTemporalDateTimeFields(calendar, fields, overflow).
   11. Let isoDate be result.[[ISODate]].
   12. Let time be result.[[Time]].
5. Else,
   1. If item is not a String, throw a TypeError exception.
   2. Let result be ? ParseISODateTime(item, « TemporalDateTimeString[+Zoned] »).
   3. Let annotation be result.[[TimeZone]].[[TimeZoneAnnotation]].
   4. Assert: annotation is not empty.
   5. Let timeZone be ? ToTemporalTimeZoneIdentifier(annotation).
   6. Let offsetString be result.[[TimeZone]].[[OffsetString]].
   7. If result.[[TimeZone]].[[Z]] is true, then
      1. Set hasUTCDesignator to true.
   8. Let calendar be result.[[Calendar]].
   9. If calendar is empty, set calendar to "iso8601".
   10. Set calendar to ? CanonicalizeCalendar(calendar).
   11. Set matchBehaviour to match-minutes.
   12. If offsetString is not empty, then
   13. Let offsetParseResult be ParseText(StringToCodePoints(offsetString), UTCOffset[+SubMinutePrecision]).
   14. Assert: offsetParseResult is a Parse Node.
   15. If offsetParseResult contains more than one MinuteSecond Parse Node, set matchBehaviour to match-exactly.
   16. Let resolvedOptions be ? GetOptionsObject(options).
   17. Let disambiguation be ? GetTemporalDisambiguationOption(resolvedOptions).
   18. Let offsetOption be ? GetTemporalOffsetOption(resolvedOptions, reject).
   19. Perform ? GetTemporalOverflowOption(resolvedOptions).
   20. Let isoDate be CreateISODateRecord(result.[[Year]], result.[[Month]], result.[[Day]]).
   21. Let time be result.[[Time]].
6. If hasUTCDesignator is true, then
   1. Let offsetBehaviour be exact.
7. Else if offsetString is empty or offsetString is unset, then
   1. Let offsetBehaviour be wall.
8. Else,
   1. Let offsetBehaviour be option.
9. Let offsetNanoseconds be 0.
10. If offsetBehaviour is option, then
    1. Set offsetNanoseconds to ! ParseDateTimeUTCOffset(offsetString).
11. Let epochNanoseconds be ? InterpretISODateTimeOffset(isoDate, time, offsetBehaviour, offsetNanoseconds, timeZone, disambiguation, offsetOption, matchBehaviour).
12. Return ! CreateTemporalZonedDateTime(epochNanoseconds, timeZone, calendar).

# 6.5.3 CreateTemporalZonedDateTime ( epochNanoseconds, timeZone, calendar [ , newTarget ] )

The abstract operation CreateTemporalZonedDateTime takes arguments epochNanoseconds (a BigInt), timeZone (an available time zone identifier), and calendar (a calendar type) and optional argument newTarget (a constructor) and returns either a normal completion containing a Temporal.ZonedDateTime or a throw completion. It creates a Temporal.ZonedDateTime instance and fills the internal slots with valid values. It performs the following steps when called:

1. Assert: IsValidEpochNanoseconds(epochNanoseconds) is true.
2. If newTarget is not present, set newTarget to %Temporal.ZonedDateTime%.
3. Let object be ? OrdinaryCreateFromConstructor(newTarget, "%Temporal.ZonedDateTime.prototype%", « [[InitializedTemporalZonedDateTime]], [[EpochNanoseconds]], [[TimeZone]], [[Calendar]] »).
4. Set object.[[EpochNanoseconds]] to epochNanoseconds.
5. Set object.[[TimeZone]] to timeZone.
6. Set object.[[Calendar]] to calendar.
7. Return object.

# 6.5.4 TemporalZonedDateTimeToString ( zonedDateTime, precision, showCalendar, showTimeZone, showOffset [ , increment [ , unit [ , roundingMode ] ] ] )

The abstract operation TemporalZonedDateTimeToString takes arguments zonedDateTime (a Temporal.ZonedDateTime), precision (either an integer between 0 and 9 inclusive, minute, or auto), showCalendar (one of auto, always, never, or critical), showTimeZone (one of auto, never, or critical), and showOffset (either auto or never) and optional arguments increment (a positive integer), unit (one of minute, second, millisecond, microsecond, or nanosecond), and roundingMode (a rounding mode) and returns a String. It returns an ISO 8601 / RFC 9557 string representation of its argument, including a time zone name annotation and calendar annotation, which are extensions to the ISO 8601 format. It performs the following steps when called:

1. If increment is not present, set increment to 1.
2. If unit is not present, set unit to nanosecond.
3. If roundingMode is not present, set roundingMode to trunc.
4. Let epochNs be zonedDateTime.[[EpochNanoseconds]].
5. Set epochNs to RoundTemporalInstant(epochNs, increment, unit, roundingMode).
6. Let timeZone be zonedDateTime.[[TimeZone]].
7. Let offsetNanoseconds be GetOffsetNanosecondsFor(timeZone, epochNs).
8. Let isoDateTime be GetISODateTimeFor(timeZone, epochNs).
9. Let dateTimeString be ISODateTimeToString(isoDateTime, "iso8601", precision, never).
10. If showOffset is never, then
    1. Let offsetString be the empty String.
11. Else,
    1. Let offsetString be FormatDateTimeUTCOffsetRounded(offsetNanoseconds).
12. If showTimeZone is never, then
    1. Let timeZoneString be the empty String.
13. Else,
    1. If showTimeZone is critical, let flag be "!"; else let flag be the empty String.
    2. Let timeZoneString be the string-concatenation of the code unit 0x005B (LEFT SQUARE BRACKET), flag, timeZone, and the code unit 0x005D (RIGHT SQUARE BRACKET).
14. Let calendarString be FormatCalendarAnnotation(zonedDateTime.[[Calendar]], showCalendar).
15. Return the string-concatenation of dateTimeString, offsetString, timeZoneString, and calendarString.

# 6.5.5 AddZonedDateTime ( epochNanoseconds, timeZone, calendar, duration, overflow )

The abstract operation AddZonedDateTime takes arguments epochNanoseconds (a BigInt), timeZone (an available time zone identifier), calendar (a calendar type), duration (an Internal Duration Record), and overflow (either constrain or reject) and returns either a normal completion containing a BigInt or a throw completion. It adds a duration in various units to a number of nanoseconds epochNanoseconds since the epoch, subject to the rules of the time zone and calendar, and returns the result as a BigInt. As specified in RFC 5545, the date portion of the duration is added in calendar days, and the time portion is added in exact time. It performs the following steps when called:

1. If DateDurationSign(duration.[[Date]]) = 0, return ? AddInstant(epochNanoseconds, duration.[[Time]]).
2. Let isoDateTime be GetISODateTimeFor(timeZone, epochNanoseconds).
3. Let addedDate be ? CalendarDateAdd(calendar, isoDateTime.[[ISODate]], duration.[[Date]], overflow).
4. Let intermediateDateTime be CombineISODateAndTimeRecord(addedDate, isoDateTime.[[Time]]).
5. If ISODateTimeWithinLimits(intermediateDateTime) is false, throw a RangeError exception.
6. Let intermediateNs be ! GetEpochNanosecondsFor(timeZone, intermediateDateTime, compatible).
7. Return ? AddInstant(intermediateNs, duration.[[Time]]).

# 6.5.6 DifferenceZonedDateTime ( ns1, ns2, timeZone, calendar, largestUnit )

The abstract operation DifferenceZonedDateTime takes arguments ns1 (a BigInt), ns2 (a BigInt), timeZone (an available time zone identifier), calendar (a calendar type), and largestUnit (a Temporal unit) and returns either a normal completion containing an Internal Duration Record, or a throw completion. It computes the difference between two exact times expressed in nanoseconds since the epoch, and balances the result so that there is no non-zero unit larger than largestUnit in the result, taking calendar reckoning and time zone offset changes into account. It performs the following steps when called:

1. If ns1 = ns2, return CombineDateAndTimeDuration(ZeroDateDuration(), 0).
2. Let startDateTime be GetISODateTimeFor(timeZone, ns1).
3. Let endDateTime be GetISODateTimeFor(timeZone, ns2).
4. If CompareISODate(startDateTime.[[ISODate]], endDateTime.[[ISODate]]) = 0, then
   1. Let timeDuration be TimeDurationFromEpochNanosecondsDifference(ns2, ns1).
   2. Return CombineDateAndTimeDuration(ZeroDateDuration(), timeDuration).
5. If ns2 \- ns1 < 0, let sign be 1; else let sign be -1.
6. If sign = -1, let maxDayCorrection be 2; else let maxDayCorrection be 1.
7. Let dayCorrection be 0.
8. Let timeDuration be DifferenceTime(startDateTime.[[Time]], endDateTime.[[Time]]).
9. If TimeDurationSign(timeDuration) = sign, set dayCorrection to dayCorrection \+ 1.
10. Let success be false.
11. Repeat, while dayCorrection ≤ maxDayCorrection and success is false,
    1. Let intermediateDate be AddDaysToISODate(endDateTime.[[ISODate]], dayCorrection × sign).
    2. Let intermediateDateTime be CombineISODateAndTimeRecord(intermediateDate, startDateTime.[[Time]]).
    3. Let intermediateNs be ? GetEpochNanosecondsFor(timeZone, intermediateDateTime, compatible).
    4. Set timeDuration to TimeDurationFromEpochNanosecondsDifference(ns2, intermediateNs).
    5. Let timeSign be TimeDurationSign(timeDuration).
    6. If sign ≠ timeSign, then
    7. Set success to true.
       7. Set dayCorrection to dayCorrection \+ 1.
12. Assert: success is true.
13. Let dateLargestUnit be LargerOfTwoTemporalUnits(largestUnit, day).
14. Let dateDifference be CalendarDateUntil(calendar, startDateTime.[[ISODate]], intermediateDateTime.[[ISODate]], dateLargestUnit).
15. Return CombineDateAndTimeDuration(dateDifference, timeDuration).

# 6.5.7 DifferenceZonedDateTimeWithRounding ( ns1, ns2, timeZone, calendar, largestUnit, roundingIncrement, smallestUnit, roundingMode )

The abstract operation DifferenceZonedDateTimeWithRounding takes arguments ns1 (a BigInt), ns2 (a BigInt), timeZone (an available time zone identifier), calendar (a calendar type), largestUnit (a Temporal unit), roundingIncrement (a positive integer), smallestUnit (a Temporal unit), and roundingMode (a rounding mode) and returns either a normal completion containing an Internal Duration Record, or a throw completion. It computes the difference between the zoned date-times represented by ns1 and ns2, rounding the result according to roundingIncrement, smallestUnit, and roundingMode. It performs the following steps when called:

1. If TemporalUnitCategory(largestUnit) is time, return DifferenceInstant(ns1, ns2, roundingIncrement, smallestUnit, roundingMode).
2. Let difference be ? DifferenceZonedDateTime(ns1, ns2, timeZone, calendar, largestUnit).
3. If smallestUnit is nanosecond and roundingIncrement = 1, return difference.
4. Let dateTime be GetISODateTimeFor(timeZone, ns1).
5. Return ? RoundRelativeDuration(difference, ns1, ns2, dateTime, timeZone, calendar, largestUnit, roundingIncrement, smallestUnit, roundingMode).

# 6.5.8 DifferenceZonedDateTimeWithTotal ( ns1, ns2, timeZone, calendar, unit )

The abstract operation DifferenceZonedDateTimeWithTotal takes arguments ns1 (a BigInt), ns2 (a BigInt), timeZone (an available time zone identifier), calendar (a calendar type), and unit (a Temporal unit) and returns either a normal completion containing a mathematical value, or a throw completion. It computes the total difference between the zoned date-times represented by ns1 and ns2 as a mathematical value in units of unit. It performs the following steps when called:

1. If TemporalUnitCategory(unit) is time, then
   1. Let difference be TimeDurationFromEpochNanosecondsDifference(ns2, ns1).
   2. Return TotalTimeDuration(difference, unit).
2. Let difference be ? DifferenceZonedDateTime(ns1, ns2, timeZone, calendar, unit).
3. Let dateTime be GetISODateTimeFor(timeZone, ns1).
4. Return ? TotalRelativeDuration(difference, ns1, ns2, dateTime, timeZone, calendar, unit).

# 6.5.9 DifferenceTemporalZonedDateTime ( operation, zonedDateTime, other, options )

The abstract operation DifferenceTemporalZonedDateTime takes arguments operation (either until or since), zonedDateTime (a Temporal.ZonedDateTime), other (an ECMAScript language value), and options (an ECMAScript language value) and returns either a normal completion containing a Temporal.Duration or a throw completion. It computes the difference between the two times represented by zonedDateTime and other, optionally rounds it, and returns it as a Temporal.Duration object. It performs the following steps when called:

1. Set other to ? ToTemporalZonedDateTime(other).
2. If CalendarEquals(zonedDateTime.[[Calendar]], other.[[Calendar]]) is false, throw a RangeError exception.
3. Let resolvedOptions be ? GetOptionsObject(options).
4. Let settings be ? GetDifferenceSettings(operation, resolvedOptions, datetime, « », nanosecond, hour).
5. If TemporalUnitCategory(settings.[[LargestUnit]]) is time, then
   1. Let internalDuration be DifferenceInstant(zonedDateTime.[[EpochNanoseconds]], other.[[EpochNanoseconds]], settings.[[RoundingIncrement]], settings.[[SmallestUnit]], settings.[[RoundingMode]]).
   2. Let result be ! TemporalDurationFromInternal(internalDuration, settings.[[LargestUnit]]).
   3. If operation is since, set result to CreateNegatedTemporalDuration(result).
   4. Return result.
6. NOTE: To calculate differences in two different time zones, settings.[[LargestUnit]] must be a time unit, because day lengths can vary between time zones due to DST and other UTC offset shifts.
7. If TimeZoneEquals(zonedDateTime.[[TimeZone]], other.[[TimeZone]]) is false, throw a RangeError exception.
8. If zonedDateTime.[[EpochNanoseconds]] = other.[[EpochNanoseconds]], return ! CreateTemporalDuration(0, 0, 0, 0, 0, 0, 0, 0, 0, 0).
9. Let internalDuration be ? DifferenceZonedDateTimeWithRounding(zonedDateTime.[[EpochNanoseconds]], other.[[EpochNanoseconds]], zonedDateTime.[[TimeZone]], zonedDateTime.[[Calendar]], settings.[[LargestUnit]], settings.[[RoundingIncrement]], settings.[[SmallestUnit]], settings.[[RoundingMode]]).
10. Let result be ! TemporalDurationFromInternal(internalDuration, hour).
11. If operation is since, set result to CreateNegatedTemporalDuration(result).
12. Return result.

# 6.5.10 AddDurationToZonedDateTime ( operation, zonedDateTime, temporalDurationLike, options )

The abstract operation AddDurationToZonedDateTime takes arguments operation (either add or subtract), zonedDateTime (a Temporal.ZonedDateTime), temporalDurationLike (an ECMAScript language value), and options (an ECMAScript language value) and returns either a normal completion containing a Temporal.ZonedDateTime or a throw completion. It adds/subtracts temporalDurationLike to/from zonedDateTime. It performs the following steps when called:

1. Let duration be ? ToTemporalDuration(temporalDurationLike).
2. If operation is subtract, set duration to CreateNegatedTemporalDuration(duration).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
5. Let calendar be zonedDateTime.[[Calendar]].
6. Let timeZone be zonedDateTime.[[TimeZone]].
7. Let internalDuration be ToInternalDurationRecord(duration).
8. Let epochNanoseconds be ? AddZonedDateTime(zonedDateTime.[[EpochNanoseconds]], timeZone, calendar, internalDuration, overflow).
9. Return ! CreateTemporalZonedDateTime(epochNanoseconds, timeZone, calendar).

# 7 Temporal.Duration Objects

A Temporal.Duration object describes the difference in elapsed time between two other Temporal objects of the same type: Instant, PlainDate, PlainDateTime, PlainTime, PlainYearMonth, or ZonedDateTime. Objects of this type are only created via the _.since()_ and _.until()_ methods of these objects.

# 7.1 The Temporal.Duration Constructor

The Temporal.Duration constructor:

- creates and initializes a new Temporal.Duration object when called as a constructor.
- is not intended to be called as a function and will throw an exception when called in that manner.
- may be used as the value of an `extends` clause of a class definition. Subclass constructors that intend to inherit the specified Temporal.Duration behaviour must include a super call to the %Temporal.Duration% constructor to create and initialize subclass instances with the necessary internal slots.

# 7.1.1 Temporal.Duration ( [ years [ , months [ , weeks [ , days [ , hours [ , minutes [ , seconds [ , milliseconds [ , microseconds [ , nanoseconds ] ] ] ] ] ] ] ] ] ] )

The `Temporal.Duration` function performs the following steps when called:

1. If NewTarget is undefined, throw a TypeError exception.
2. If years is undefined, let y be 0; else let y be ? ToIntegerIfIntegral(years).
3. If months is undefined, let mo be 0; else let mo be ? ToIntegerIfIntegral(months).
4. If weeks is undefined, let w be 0; else let w be ? ToIntegerIfIntegral(weeks).
5. If days is undefined, let d be 0; else let d be ? ToIntegerIfIntegral(days).
6. If hours is undefined, let h be 0; else let h be ? ToIntegerIfIntegral(hours).
7. If minutes is undefined, let m be 0; else let m be ? ToIntegerIfIntegral(minutes).
8. If seconds is undefined, let s be 0; else let s be ? ToIntegerIfIntegral(seconds).
9. If milliseconds is undefined, let ms be 0; else let ms be ? ToIntegerIfIntegral(milliseconds).
10. If microseconds is undefined, let mis be 0; else let mis be ? ToIntegerIfIntegral(microseconds).
11. If nanoseconds is undefined, let ns be 0; else let ns be ? ToIntegerIfIntegral(nanoseconds).
12. Return ? CreateTemporalDuration(y, mo, w, d, h, m, s, ms, mis, ns, NewTarget).

# 7.2 Properties of the Temporal.Duration Constructor

The value of the [[Prototype]] internal slot of the Temporal.Duration constructor is the intrinsic object %Function.prototype%.

The Temporal.Duration constructor has the following properties:

# 7.2.1 Temporal.Duration.prototype

The initial value of `Temporal.Duration.prototype` is %Temporal.Duration.prototype%.

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: false }.

# 7.2.2 Temporal.Duration.from ( item )

The `Temporal.Duration.from` function performs the following steps when called:

1. Return ? ToTemporalDuration(item).

# 7.2.3 Temporal.Duration.compare ( one, two [ , options ] )

The `Temporal.Duration.compare` function performs the following steps when called:

1. Set one to ? ToTemporalDuration(one).
2. Set two to ? ToTemporalDuration(two).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. Let relativeToRecord be ? GetTemporalRelativeToOption(resolvedOptions).
5. If one.[[Years]] = two.[[Years]], and one.[[Months]] = two.[[Months]], and one.[[Weeks]] = two.[[Weeks]], and one.[[Days]] = two.[[Days]], and one.[[Hours]] = two.[[Hours]], and one.[[Minutes]] = two.[[Minutes]], and one.[[Seconds]] = two.[[Seconds]], and one.[[Milliseconds]] = two.[[Milliseconds]], and one.[[Microseconds]] = two.[[Microseconds]], and one.[[Nanoseconds]] = two.[[Nanoseconds]], then
   1. Return +0𝔽.
6. Let zonedRelativeTo be relativeToRecord.[[ZonedRelativeTo]].
7. Let plainRelativeTo be relativeToRecord.[[PlainRelativeTo]].
8. Let largestUnit1 be DefaultTemporalLargestUnit(one).
9. Let largestUnit2 be DefaultTemporalLargestUnit(two).
10. Let duration1 be ToInternalDurationRecord(one).
11. Let duration2 be ToInternalDurationRecord(two).
12. If zonedRelativeTo is not undefined, and TemporalUnitCategory(largestUnit1) is date or TemporalUnitCategory(largestUnit2) is date, then
    1. Let timeZone be zonedRelativeTo.[[TimeZone]].
    2. Let calendar be zonedRelativeTo.[[Calendar]].
    3. Let after1 be ? AddZonedDateTime(zonedRelativeTo.[[EpochNanoseconds]], timeZone, calendar, duration1, constrain).
    4. Let after2 be ? AddZonedDateTime(zonedRelativeTo.[[EpochNanoseconds]], timeZone, calendar, duration2, constrain).
    5. If after1 > after2, return 1𝔽.
    6. If after1 < after2, return -1𝔽.
    7. Return +0𝔽.
13. If IsCalendarUnit(largestUnit1) is true or IsCalendarUnit(largestUnit2) is true, then
    1. If plainRelativeTo is undefined, throw a RangeError exception.
    2. Let days1 be ? DateDurationDays(duration1.[[Date]], plainRelativeTo).
    3. Let days2 be ? DateDurationDays(duration2.[[Date]], plainRelativeTo).
14. Else,
    1. Let days1 be one.[[Days]].
    2. Let days2 be two.[[Days]].
15. Let timeDuration1 be ? Add24HourDaysToTimeDuration(duration1.[[Time]], days1).
16. Let timeDuration2 be ? Add24HourDaysToTimeDuration(duration2.[[Time]], days2).
17. Return 𝔽(CompareTimeDuration(timeDuration1, timeDuration2)).

# 7.3 Properties of the Temporal.Duration Prototype Object

The Temporal.Duration prototype object

- is itself an ordinary object.
- is not a Temporal.Duration instance and doesn't have an [[InitializedTemporalDuration]] internal slot.
- has a [[Prototype]] internal slot whose value is %Object.prototype%.

# 7.3.1 Temporal.Duration.prototype.constructor

The initial value of `Temporal.Duration.prototype.constructor` is %Temporal.Duration%.

# 7.3.2 Temporal.Duration.prototype[ %Symbol.toStringTag% ]

The initial value of the %Symbol.toStringTag% property is the String "Temporal.Duration".

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }.

# 7.3.3 get Temporal.Duration.prototype.years

`Temporal.Duration.prototype.years` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(duration.[[Years]]).

# 7.3.4 get Temporal.Duration.prototype.months

`Temporal.Duration.prototype.months` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(duration.[[Months]]).

# 7.3.5 get Temporal.Duration.prototype.weeks

`Temporal.Duration.prototype.weeks` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(duration.[[Weeks]]).

# 7.3.6 get Temporal.Duration.prototype.days

`Temporal.Duration.prototype.days` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(duration.[[Days]]).

# 7.3.7 get Temporal.Duration.prototype.hours

`Temporal.Duration.prototype.hours` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(duration.[[Hours]]).

# 7.3.8 get Temporal.Duration.prototype.minutes

`Temporal.Duration.prototype.minutes` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(duration.[[Minutes]]).

# 7.3.9 get Temporal.Duration.prototype.seconds

`Temporal.Duration.prototype.seconds` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(duration.[[Seconds]]).

# 7.3.10 get Temporal.Duration.prototype.milliseconds

`Temporal.Duration.prototype.milliseconds` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(duration.[[Milliseconds]]).

# 7.3.11 get Temporal.Duration.prototype.microseconds

`Temporal.Duration.prototype.microseconds` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(duration.[[Microseconds]]).

# 7.3.12 get Temporal.Duration.prototype.nanoseconds

`Temporal.Duration.prototype.nanoseconds` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(duration.[[Nanoseconds]]).

# 7.3.13 get Temporal.Duration.prototype.sign

`Temporal.Duration.prototype.sign` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return 𝔽(DurationSign(duration)).

# 7.3.14 get Temporal.Duration.prototype.blank

`Temporal.Duration.prototype.blank` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. If DurationSign(duration) = 0, return true.
4. Return false.

# 7.3.15 Temporal.Duration.prototype.with ( temporalDurationLike )

The `Temporal.Duration.prototype.with` method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Let temporalDurationLike be ? ToTemporalPartialDurationRecord(temporalDurationLike).
4. If temporalDurationLike.[[Years]] is not undefined, then
   1. Let years be temporalDurationLike.[[Years]].
5. Else,
   1. Let years be duration.[[Years]].
6. If temporalDurationLike.[[Months]] is not undefined, then
   1. Let months be temporalDurationLike.[[Months]].
7. Else,
   1. Let months be duration.[[Months]].
8. If temporalDurationLike.[[Weeks]] is not undefined, then
   1. Let weeks be temporalDurationLike.[[Weeks]].
9. Else,
   1. Let weeks be duration.[[Weeks]].
10. If temporalDurationLike.[[Days]] is not undefined, then
    1. Let days be temporalDurationLike.[[Days]].
11. Else,
    1. Let days be duration.[[Days]].
12. If temporalDurationLike.[[Hours]] is not undefined, then
    1. Let hours be temporalDurationLike.[[Hours]].
13. Else,
    1. Let hours be duration.[[Hours]].
14. If temporalDurationLike.[[Minutes]] is not undefined, then
    1. Let minutes be temporalDurationLike.[[Minutes]].
15. Else,
    1. Let minutes be duration.[[Minutes]].
16. If temporalDurationLike.[[Seconds]] is not undefined, then
    1. Let seconds be temporalDurationLike.[[Seconds]].
17. Else,
    1. Let seconds be duration.[[Seconds]].
18. If temporalDurationLike.[[Milliseconds]] is not undefined, then
    1. Let milliseconds be temporalDurationLike.[[Milliseconds]].
19. Else,
    1. Let milliseconds be duration.[[Milliseconds]].
20. If temporalDurationLike.[[Microseconds]] is not undefined, then
    1. Let microseconds be temporalDurationLike.[[Microseconds]].
21. Else,
    1. Let microseconds be duration.[[Microseconds]].
22. If temporalDurationLike.[[Nanoseconds]] is not undefined, then
    1. Let nanoseconds be temporalDurationLike.[[Nanoseconds]].
23. Else,
    1. Let nanoseconds be duration.[[Nanoseconds]].
24. Return ? CreateTemporalDuration(years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds).

# 7.3.16 Temporal.Duration.prototype.negated ( )

The `Temporal.Duration.prototype.negated` method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return CreateNegatedTemporalDuration(duration).

# 7.3.17 Temporal.Duration.prototype.abs ( )

The `Temporal.Duration.prototype.abs` method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return ! CreateTemporalDuration(abs(duration.[[Years]]), abs(duration.[[Months]]), abs(duration.[[Weeks]]), abs(duration.[[Days]]), abs(duration.[[Hours]]), abs(duration.[[Minutes]]), abs(duration.[[Seconds]]), abs(duration.[[Milliseconds]]), abs(duration.[[Microseconds]]), abs(duration.[[Nanoseconds]])).

# 7.3.18 Temporal.Duration.prototype.add ( other )

The `Temporal.Duration.prototype.add` method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return ? AddDurations(add, duration, other).

# 7.3.19 Temporal.Duration.prototype.subtract ( other )

The `Temporal.Duration.prototype.subtract` method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return ? AddDurations(subtract, duration, other).

# 7.3.20 Temporal.Duration.prototype.round ( roundTo )

The `Temporal.Duration.prototype.round` method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. If roundTo is undefined, throw a TypeError exception.
4. If roundTo is a String, then
   1. Let paramString be roundTo.
   2. Set roundTo to OrdinaryObjectCreate(null).
   3. Perform ! CreateDataPropertyOrThrow(roundTo, "smallestUnit", paramString).
5. Else,
   1. Set roundTo to ? GetOptionsObject(roundTo).
6. Let smallestUnitPresent be true.
7. Let largestUnitPresent be true.
8. NOTE: The following steps read options and perform independent validation in alphabetical order (GetTemporalRelativeToOption reads "relativeTo", GetRoundingIncrementOption reads "roundingIncrement" and GetRoundingModeOption reads "roundingMode").
9. Let largestUnit be ? GetTemporalUnitValuedOption(roundTo, "largestUnit", unset).
10. Let relativeToRecord be ? GetTemporalRelativeToOption(roundTo).
11. Let zonedRelativeTo be relativeToRecord.[[ZonedRelativeTo]].
12. Let plainRelativeTo be relativeToRecord.[[PlainRelativeTo]].
13. Let roundingIncrement be ? GetRoundingIncrementOption(roundTo).
14. Let roundingMode be ? GetRoundingModeOption(roundTo, half-expand).
15. Let smallestUnit be ? GetTemporalUnitValuedOption(roundTo, "smallestUnit", unset).
16. Perform ? ValidateTemporalUnitValue(smallestUnit, datetime).
17. If smallestUnit is unset, then
    1. Set smallestUnitPresent to false.
    2. Set smallestUnit to nanosecond.
18. Let existingLargestUnit be DefaultTemporalLargestUnit(duration).
19. Let defaultLargestUnit be LargerOfTwoTemporalUnits(existingLargestUnit, smallestUnit).
20. If largestUnit is unset, then
    1. Set largestUnitPresent to false.
    2. Set largestUnit to defaultLargestUnit.
21. Else if largestUnit is auto, then
    1. Set largestUnit to defaultLargestUnit.
22. If smallestUnitPresent is false and largestUnitPresent is false, throw a RangeError exception.
23. If LargerOfTwoTemporalUnits(largestUnit, smallestUnit) is not largestUnit, throw a RangeError exception.
24. Let maximum be MaximumTemporalDurationRoundingIncrement(smallestUnit).
25. If maximum is not unset, perform ? ValidateTemporalRoundingIncrement(roundingIncrement, maximum, false).
26. If roundingIncrement > 1, and largestUnit is not smallestUnit, and TemporalUnitCategory(smallestUnit) is date, throw a RangeError exception.
27. If zonedRelativeTo is not undefined, then
    1. Let internalDuration be ToInternalDurationRecord(duration).
    2. Let timeZone be zonedRelativeTo.[[TimeZone]].
    3. Let calendar be zonedRelativeTo.[[Calendar]].
    4. Let relativeEpochNs be zonedRelativeTo.[[EpochNanoseconds]].
    5. Let targetEpochNs be ? AddZonedDateTime(relativeEpochNs, timeZone, calendar, internalDuration, constrain).
    6. Set internalDuration to ? DifferenceZonedDateTimeWithRounding(relativeEpochNs, targetEpochNs, timeZone, calendar, largestUnit, roundingIncrement, smallestUnit, roundingMode).
    7. If TemporalUnitCategory(largestUnit) is date, set largestUnit to hour.
    8. Return ? TemporalDurationFromInternal(internalDuration, largestUnit).
28. If plainRelativeTo is not undefined, then
    1. Let internalDuration be ToInternalDurationRecordWith24HourDays(duration).
    2. Let targetTime be AddTime(MidnightTimeRecord(), internalDuration.[[Time]]).
    3. Let calendar be plainRelativeTo.[[Calendar]].
    4. Let dateDuration be ! AdjustDateDurationRecord(internalDuration.[[Date]], targetTime.[[Days]]).
    5. Let targetDate be ? CalendarDateAdd(calendar, plainRelativeTo.[[ISODate]], dateDuration, constrain).
    6. Let isoDateTime be CombineISODateAndTimeRecord(plainRelativeTo.[[ISODate]], MidnightTimeRecord()).
    7. Let targetDateTime be CombineISODateAndTimeRecord(targetDate, targetTime).
    8. Set internalDuration to ? DifferencePlainDateTimeWithRounding(isoDateTime, targetDateTime, calendar, largestUnit, roundingIncrement, smallestUnit, roundingMode).
    9. Return ? TemporalDurationFromInternal(internalDuration, largestUnit).
29. If IsCalendarUnit(existingLargestUnit) is true or IsCalendarUnit(largestUnit) is true, throw a RangeError exception.
30. Assert: IsCalendarUnit(smallestUnit) is false.
31. Let internalDuration be ToInternalDurationRecordWith24HourDays(duration).
32. If smallestUnit is day, then
    1. Let fractionalDays be TotalTimeDuration(internalDuration.[[Time]], day).
    2. Let days be RoundNumberToIncrement(fractionalDays, roundingIncrement, roundingMode).
    3. Let dateDuration be ? CreateDateDurationRecord(0, 0, 0, days).
    4. Set internalDuration to CombineDateAndTimeDuration(dateDuration, 0).
33. Else,
    1. Let timeDuration be ? RoundTimeDuration(internalDuration.[[Time]], roundingIncrement, smallestUnit, roundingMode).
    2. Set internalDuration to CombineDateAndTimeDuration(ZeroDateDuration(), timeDuration).
34. Return ? TemporalDurationFromInternal(internalDuration, largestUnit).

# 7.3.21 Temporal.Duration.prototype.total ( totalOf )

The `Temporal.Duration.prototype.total` method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. If totalOf is undefined, throw a TypeError exception.
4. If totalOf is a String, then
   1. Let paramString be totalOf.
   2. Set totalOf to OrdinaryObjectCreate(null).
   3. Perform ! CreateDataPropertyOrThrow(totalOf, "unit", paramString).
5. Else,
   1. Set totalOf to ? GetOptionsObject(totalOf).
6. NOTE: The following steps read options and perform independent validation in alphabetical order (GetTemporalRelativeToOption reads "relativeTo").
7. Let relativeToRecord be ? GetTemporalRelativeToOption(totalOf).
8. Let zonedRelativeTo be relativeToRecord.[[ZonedRelativeTo]].
9. Let plainRelativeTo be relativeToRecord.[[PlainRelativeTo]].
10. Let unit be ? GetTemporalUnitValuedOption(totalOf, "unit", required).
11. Perform ? ValidateTemporalUnitValue(unit, datetime).
12. If zonedRelativeTo is not undefined, then
    1. Let internalDuration be ToInternalDurationRecord(duration).
    2. Let timeZone be zonedRelativeTo.[[TimeZone]].
    3. Let calendar be zonedRelativeTo.[[Calendar]].
    4. Let relativeEpochNs be zonedRelativeTo.[[EpochNanoseconds]].
    5. Let targetEpochNs be ? AddZonedDateTime(relativeEpochNs, timeZone, calendar, internalDuration, constrain).
    6. Let total be ? DifferenceZonedDateTimeWithTotal(relativeEpochNs, targetEpochNs, timeZone, calendar, unit).
13. Else if plainRelativeTo is not undefined, then
    1. Let internalDuration be ToInternalDurationRecordWith24HourDays(duration).
    2. Let targetTime be AddTime(MidnightTimeRecord(), internalDuration.[[Time]]).
    3. Let calendar be plainRelativeTo.[[Calendar]].
    4. Let dateDuration be ! AdjustDateDurationRecord(internalDuration.[[Date]], targetTime.[[Days]]).
    5. Let targetDate be ? CalendarDateAdd(calendar, plainRelativeTo.[[ISODate]], dateDuration, constrain).
    6. Let isoDateTime be CombineISODateAndTimeRecord(plainRelativeTo.[[ISODate]], MidnightTimeRecord()).
    7. Let targetDateTime be CombineISODateAndTimeRecord(targetDate, targetTime).
    8. Let total be ? DifferencePlainDateTimeWithTotal(isoDateTime, targetDateTime, calendar, unit).
14. Else,
    1. Let largestUnit be DefaultTemporalLargestUnit(duration).
    2. If IsCalendarUnit(largestUnit) is true or IsCalendarUnit(unit) is true, throw a RangeError exception.
    3. Let internalDuration be ToInternalDurationRecordWith24HourDays(duration).
    4. Let total be TotalTimeDuration(internalDuration.[[Time]], unit).
15. Return 𝔽(total).

# 7.3.22 Temporal.Duration.prototype.toString ( [ options ] )

The `Temporal.Duration.prototype.toString` method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. NOTE: The following steps read options and perform independent validation in alphabetical order (GetTemporalFractionalSecondDigitsOption reads "fractionalSecondDigits" and GetRoundingModeOption reads "roundingMode").
5. Let digits be ? GetTemporalFractionalSecondDigitsOption(resolvedOptions).
6. Let roundingMode be ? GetRoundingModeOption(resolvedOptions, trunc).
7. Let smallestUnit be ? GetTemporalUnitValuedOption(resolvedOptions, "smallestUnit", unset).
8. Perform ? ValidateTemporalUnitValue(smallestUnit, time).
9. If smallestUnit is either hour or minute, throw a RangeError exception.
10. Let precision be ToSecondsStringPrecisionRecord(smallestUnit, digits).
11. If precision.[[Unit]] is nanosecond and precision.[[Increment]] = 1, then
    1. Return TemporalDurationToString(duration, precision.[[Precision]]).
12. Let largestUnit be DefaultTemporalLargestUnit(duration).
13. Let internalDuration be ToInternalDurationRecord(duration).
14. Let timeDuration be ? RoundTimeDuration(internalDuration.[[Time]], precision.[[Increment]], precision.[[Unit]], roundingMode).
15. Set internalDuration to CombineDateAndTimeDuration(internalDuration.[[Date]], timeDuration).
16. Let roundedLargestUnit be LargerOfTwoTemporalUnits(largestUnit, second).
17. Let roundedDuration be ? TemporalDurationFromInternal(internalDuration, roundedLargestUnit).
18. Return TemporalDurationToString(roundedDuration, precision.[[Precision]]).

# 7.3.23 Temporal.Duration.prototype.toJSON ( )

The `Temporal.Duration.prototype.toJSON` method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return TemporalDurationToString(duration, auto).

# 7.3.24 Temporal.Duration.prototype.toLocaleString ( [ locales [ , options ] ] )

An ECMAScript implementation that includes the ECMA-402 Internationalization API must implement the `Temporal.Duration.prototype.toLocaleString` method as specified in the ECMA-402 specification. If an ECMAScript implementation does not include the ECMA-402 API the following specification of the `Temporal.Duration.prototype.toLocaleString` method is used.

The `Temporal.Duration.prototype.toLocaleString` method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Return TemporalDurationToString(duration, auto).

# 7.3.25 Temporal.Duration.prototype.valueOf ( )

The `Temporal.Duration.prototype.valueOf` method performs the following steps when called:

1. Throw a TypeError exception.

Note

This method always throws, because in the absence of `valueOf()`, expressions with arithmetic operators such as `duration1 > duration2` would fall back to being equivalent to `duration1.toString() > duration2.toString()`. Lexicographical comparison of serialized strings might not seem obviously wrong, because the result would sometimes be correct. Implementations are encouraged to phrase the error message to point users to `Temporal.Duration.compare()` and/or `Temporal.Duration.prototype.toString()`.

# 7.4 Properties of Temporal.Duration Instances

Temporal.Duration instances are ordinary objects that inherit properties from the %Temporal.Duration.prototype% intrinsic object. Temporal.Duration instances are initially created with the internal slots described in Table 9.

A float64-representable integer is an integer that is exactly representable as a Number. That is, for a float64-representable integer x, it must hold that ℝ(𝔽(x)) = x.

Note

The use of float64-representable integers here is intended so that implementations can store and do arithmetic on Duration fields using 64-bit floating-point values.

| Table 9: Internal Slots of Temporal.Duration Instances Internal Slot | Description                                                                                               |
| -------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| [[InitializedTemporalDuration]]                                      | The only specified use of this slot is for distinguishing Temporal.Duration instances from other objects. |
| [[Years]]                                                            | A float64-representable integer representing the number of years in the duration.                         |
| [[Months]]                                                           | A float64-representable integer representing the number of months in the duration.                        |
| [[Weeks]]                                                            | A float64-representable integer representing the number of weeks in the duration.                         |
| [[Days]]                                                             | A float64-representable integer representing the number of days in the duration.                          |
| [[Hours]]                                                            | A float64-representable integer representing the number of hours in the duration.                         |
| [[Minutes]]                                                          | A float64-representable integer representing the number of minutes in the duration.                       |
| [[Seconds]]                                                          | A float64-representable integer representing the number of seconds in the duration.                       |
| [[Milliseconds]]                                                     | A float64-representable integer representing the number of milliseconds in the duration.                  |
| [[Microseconds]]                                                     | A float64-representable integer representing the number of microseconds in the duration.                  |
| [[Nanoseconds]]                                                      | A float64-representable integer representing the number of nanoseconds in the duration.                   |

# 7.5 Abstract Operations

# 7.5.1 Date Duration Records

A Date Duration Record is a Record value used to represent the portion of a Temporal.Duration object that deals with calendar date units. Date Duration Records are produced by the abstract operation CreateDateDurationRecord, among others.

Date Duration Records have the fields listed in Table 10.

| Table 10: Date Duration Record Fields Field Name | Value                           | Meaning                               |
| ------------------------------------------------ | ------------------------------- | ------------------------------------- |
| [[Years]]                                        | a float64-representable integer | The number of years in the duration.  |
| [[Months]]                                       | a float64-representable integer | The number of months in the duration. |
| [[Weeks]]                                        | a float64-representable integer | The number of weeks in the duration.  |
| [[Days]]                                         | a float64-representable integer | The number of days in the duration.   |

# 7.5.2 Partial Duration Records

A partial Duration Record is a Record value used to represent a portion of a Temporal.Duration object, in which it is not required that all the fields be specified.

Partial Duration Records have the fields listed in Table 11. Additionally, Partial Duration Records must have at least one field that is not undefined.

| Table 11: Partial Duration Record Fields Field Name | Value                                        | Meaning                                     |
| --------------------------------------------------- | -------------------------------------------- | ------------------------------------------- |
| [[Years]]                                           | a float64-representable integer or undefined | The number of years in the duration.        |
| [[Months]]                                          | a float64-representable integer or undefined | The number of months in the duration.       |
| [[Weeks]]                                           | a float64-representable integer or undefined | The number of weeks in the duration.        |
| [[Days]]                                            | a float64-representable integer or undefined | The number of days in the duration.         |
| [[Hours]]                                           | a float64-representable integer or undefined | The number of hours in the duration.        |
| [[Minutes]]                                         | a float64-representable integer or undefined | The number of minutes in the duration.      |
| [[Seconds]]                                         | a float64-representable integer or undefined | The number of seconds in the duration.      |
| [[Milliseconds]]                                    | a float64-representable integer or undefined | The number of milliseconds in the duration. |
| [[Microseconds]]                                    | a float64-representable integer or undefined | The number of microseconds in the duration. |
| [[Nanoseconds]]                                     | a float64-representable integer or undefined | The number of nanoseconds in the duration.  |

# 7.5.3 Internal Duration Records

A Internal Duration Record is a Record value used to represent the combination of a Date Duration Record with a time duration. Such Records are used by operations that deal with both date and time portions of durations, such as RoundTimeDuration.

A time duration is an integer in the inclusive interval from -maxTimeDuration to maxTimeDuration, where maxTimeDuration = 253 × 109 \- 1 = 9,007,199,254,740,991,999,999,999 It represents the portion of a Temporal.Duration object that deals with time units, but as a combined value of total nanoseconds.

Internal Duration Records have the fields listed in Table 12.

| Table 12: Internal Duration Record Fields Field Name | Value                  | Meaning                           |
| ---------------------------------------------------- | ---------------------- | --------------------------------- |
| [[Date]]                                             | a Date Duration Record | The date portion of the duration. |
| [[Time]]                                             | a time duration        | The time portion of the duration. |

# 7.5.4 ZeroDateDuration ( )

The abstract operation ZeroDateDuration takes no arguments and returns a Date Duration Record. The returned Record represents a duration with length 0. It performs the following steps when called:

1. Return ! CreateDateDurationRecord(0, 0, 0, 0).

# 7.5.5 ToInternalDurationRecord ( duration )

The abstract operation ToInternalDurationRecord takes argument duration (a Temporal.Duration) and returns an Internal Duration Record. It converts duration into its internal form, for use in duration calculations that may involve time zones. The duration's days are kept separate and not converted into the [[Time]] field. It performs the following steps when called:

1. Let dateDuration be ! CreateDateDurationRecord(duration.[[Years]], duration.[[Months]], duration.[[Weeks]], duration.[[Days]]).
2. Let timeDuration be TimeDurationFromComponents(duration.[[Hours]], duration.[[Minutes]], duration.[[Seconds]], duration.[[Milliseconds]], duration.[[Microseconds]], duration.[[Nanoseconds]]).
3. Return CombineDateAndTimeDuration(dateDuration, timeDuration).

# 7.5.6 ToInternalDurationRecordWith24HourDays ( duration )

The abstract operation ToInternalDurationRecordWith24HourDays takes argument duration (a Temporal.Duration) and returns an Internal Duration Record. It converts duration into its internal form, for use in duration calculations that do not involve time zones. The duration's days are assumed to be uniformly 24 hours, and the [[Date]].[[Days]] field of the returned Record is set to 0, while the [[Time]] field includes the days. It performs the following steps when called:

1. Let timeDuration be TimeDurationFromComponents(duration.[[Hours]], duration.[[Minutes]], duration.[[Seconds]], duration.[[Milliseconds]], duration.[[Microseconds]], duration.[[Nanoseconds]]).
2. Set timeDuration to ! Add24HourDaysToTimeDuration(timeDuration, duration.[[Days]]).
3. Let dateDuration be ! CreateDateDurationRecord(duration.[[Years]], duration.[[Months]], duration.[[Weeks]], 0).
4. Return CombineDateAndTimeDuration(dateDuration, timeDuration).

# 7.5.7 ToDateDurationRecordWithoutTime ( duration )

The abstract operation ToDateDurationRecordWithoutTime takes argument duration (a Temporal.Duration) and returns a Date Duration Record. It converts duration into only a date part, for use in date-only calculations. The duration's days are assumed to be uniformly 24 hours, and the [[Days]] field of the returned Record is set to the [[Days]] internal slot of duration, incremented by any whole days that duration's time units added up to. It performs the following steps when called:

1. Let internalDuration be ToInternalDurationRecordWith24HourDays(duration).
2. Let days be truncate(internalDuration.[[Time]] / nsPerDay).
3. Return ! CreateDateDurationRecord(internalDuration.[[Date]].[[Years]], internalDuration.[[Date]].[[Months]], internalDuration.[[Date]].[[Weeks]], days).

# 7.5.8 TemporalDurationFromInternal ( internalDuration, largestUnit )

The abstract operation TemporalDurationFromInternal takes arguments internalDuration (an Internal Duration Record) and largestUnit (a Temporal unit) and returns either a normal completion containing a Temporal.Duration, or a throw completion. It converts internalDuration back into the form of a `Temporal.Duration` object, with each component stored separately. The time units are balanced up to largestUnit. The conversion may be lossy if largestUnit is millisecond, microsecond, or nanosecond. In that case, the internal slots of the returned Temporal.Duration may contain unsafe (but float64-representable) integers. The result of a lossy conversion may be outside the allowed range for Durations, even if the input was not. It performs the following steps when called:

1. Let days, hours, minutes, seconds, milliseconds, and microseconds be 0.
2. Let sign be TimeDurationSign(internalDuration.[[Time]]).
3. Let nanoseconds be abs(internalDuration.[[Time]]).
4. If TemporalUnitCategory(largestUnit) is date, then
   1. Set microseconds to floor(nanoseconds / 1000).
   2. Set nanoseconds to nanoseconds modulo 1000.
   3. Set milliseconds to floor(microseconds / 1000).
   4. Set microseconds to microseconds modulo 1000.
   5. Set seconds to floor(milliseconds / 1000).
   6. Set milliseconds to milliseconds modulo 1000.
   7. Set minutes to floor(seconds / 60).
   8. Set seconds to seconds modulo 60.
   9. Set hours to floor(minutes / 60).
   10. Set minutes to minutes modulo 60.
   11. Set days to floor(hours / 24).
   12. Set hours to hours modulo 24.
5. Else if largestUnit is hour, then
   1. Set microseconds to floor(nanoseconds / 1000).
   2. Set nanoseconds to nanoseconds modulo 1000.
   3. Set milliseconds to floor(microseconds / 1000).
   4. Set microseconds to microseconds modulo 1000.
   5. Set seconds to floor(milliseconds / 1000).
   6. Set milliseconds to milliseconds modulo 1000.
   7. Set minutes to floor(seconds / 60).
   8. Set seconds to seconds modulo 60.
   9. Set hours to floor(minutes / 60).
   10. Set minutes to minutes modulo 60.
6. Else if largestUnit is minute, then
   1. Set microseconds to floor(nanoseconds / 1000).
   2. Set nanoseconds to nanoseconds modulo 1000.
   3. Set milliseconds to floor(microseconds / 1000).
   4. Set microseconds to microseconds modulo 1000.
   5. Set seconds to floor(milliseconds / 1000).
   6. Set milliseconds to milliseconds modulo 1000.
   7. Set minutes to floor(seconds / 60).
   8. Set seconds to seconds modulo 60.
7. Else if largestUnit is second, then
   1. Set microseconds to floor(nanoseconds / 1000).
   2. Set nanoseconds to nanoseconds modulo 1000.
   3. Set milliseconds to floor(microseconds / 1000).
   4. Set microseconds to microseconds modulo 1000.
   5. Set seconds to floor(milliseconds / 1000).
   6. Set milliseconds to milliseconds modulo 1000.
8. Else if largestUnit is millisecond, then
   1. Set microseconds to floor(nanoseconds / 1000).
   2. Set nanoseconds to nanoseconds modulo 1000.
   3. Set milliseconds to floor(microseconds / 1000).
   4. Set microseconds to microseconds modulo 1000.
9. Else if largestUnit is microsecond, then
   1. Set microseconds to floor(nanoseconds / 1000).
   2. Set nanoseconds to nanoseconds modulo 1000.
10. Else,
    1. Assert: largestUnit is nanosecond.
11. NOTE: When largestUnit is millisecond, microsecond, or nanosecond, milliseconds, microseconds, or nanoseconds may be an unsafe integer. In this case, care must be taken when implementing the calculation using floating point arithmetic. It can be implemented in C++ using `std::fma()`. String manipulation will also give an exact result, since the multiplication is by a power of 10.
12. Return ? CreateTemporalDuration(internalDuration.[[Date]].[[Years]], internalDuration.[[Date]].[[Months]], internalDuration.[[Date]].[[Weeks]], internalDuration.[[Date]].[[Days]] \+ days × sign, hours × sign, minutes × sign, seconds × sign, milliseconds × sign, microseconds × sign, nanoseconds × sign).

# 7.5.9 CreateDateDurationRecord ( years, months, weeks, days )

The abstract operation CreateDateDurationRecord takes arguments years (an integer), months (an integer), weeks (an integer), and days (an integer) and returns either a normal completion containing a Date Duration Record or a throw completion. It creates a Date Duration Record from the given date components, or throws a RangeError if the resulting duration would be invalid. It performs the following steps when called:

1. If IsValidDuration(years, months, weeks, days, 0, 0, 0, 0, 0, 0) is false, throw a RangeError exception.
2. Return Date Duration Record { [[Years]]: ℝ(𝔽(years)), [[Months]]: ℝ(𝔽(months)), [[Weeks]]: ℝ(𝔽(weeks)), [[Days]]: ℝ(𝔽(days)) }.

# 7.5.10 AdjustDateDurationRecord ( dateDuration, days [ , weeks [ , months ] ] )

The abstract operation AdjustDateDurationRecord takes arguments dateDuration (a Date Duration Record) and days (an integer) and optional arguments weeks (an integer) and months (an integer) and returns either a normal completion containing a Date Duration Record or a throw completion. It creates a new Date Duration Record that is a copy of dateDuration, with one or more fields replaced with new values. It performs the following steps when called:

1. If weeks is not present, set weeks to dateDuration.[[Weeks]].
2. If months is not present, set months to dateDuration.[[Months]].
3. Return ? CreateDateDurationRecord(dateDuration.[[Years]], months, weeks, days).

# 7.5.11 CombineDateAndTimeDuration ( dateDuration, timeDuration )

The abstract operation CombineDateAndTimeDuration takes arguments dateDuration (a Date Duration Record) and timeDuration (a time duration) and returns an Internal Duration Record. It combines dateDuration and timeDuration into an Internal Duration Record. It performs the following steps when called:

1. Let dateSign be DateDurationSign(dateDuration).
2. Let timeSign be TimeDurationSign(timeDuration).
3. Assert: If dateSign ≠ 0 and timeSign ≠ 0, dateSign = timeSign.
4. Return Internal Duration Record { [[Date]]: dateDuration, [[Time]]: timeDuration }.

# 7.5.12 ToTemporalDuration ( item )

The abstract operation ToTemporalDuration takes argument item (an ECMAScript language value) and returns either a normal completion containing a Temporal.Duration or a throw completion. Converts item to a new Temporal.Duration instance if possible and returns that, and throws otherwise. It performs the following steps when called:

1. If item is an Object and item has an [[InitializedTemporalDuration]] internal slot, then
   1. Return ! CreateTemporalDuration(item.[[Years]], item.[[Months]], item.[[Weeks]], item.[[Days]], item.[[Hours]], item.[[Minutes]], item.[[Seconds]], item.[[Milliseconds]], item.[[Microseconds]], item.[[Nanoseconds]]).
2. If item is not an Object, then
   1. If item is not a String, throw a TypeError exception.
   2. Return ? ParseTemporalDurationString(item).
3. Let result be a new Partial Duration Record with each field set to 0.
4. Let partial be ? ToTemporalPartialDurationRecord(item).
5. If partial.[[Years]] is not undefined, set result.[[Years]] to partial.[[Years]].
6. If partial.[[Months]] is not undefined, set result.[[Months]] to partial.[[Months]].
7. If partial.[[Weeks]] is not undefined, set result.[[Weeks]] to partial.[[Weeks]].
8. If partial.[[Days]] is not undefined, set result.[[Days]] to partial.[[Days]].
9. If partial.[[Hours]] is not undefined, set result.[[Hours]] to partial.[[Hours]].
10. If partial.[[Minutes]] is not undefined, set result.[[Minutes]] to partial.[[Minutes]].
11. If partial.[[Seconds]] is not undefined, set result.[[Seconds]] to partial.[[Seconds]].
12. If partial.[[Milliseconds]] is not undefined, set result.[[Milliseconds]] to partial.[[Milliseconds]].
13. If partial.[[Microseconds]] is not undefined, set result.[[Microseconds]] to partial.[[Microseconds]].
14. If partial.[[Nanoseconds]] is not undefined, set result.[[Nanoseconds]] to partial.[[Nanoseconds]].
15. Return ? CreateTemporalDuration(result.[[Years]], result.[[Months]], result.[[Weeks]], result.[[Days]], result.[[Hours]], result.[[Minutes]], result.[[Seconds]], result.[[Milliseconds]], result.[[Microseconds]], result.[[Nanoseconds]]).

# 7.5.13 DurationSign ( duration )

The abstract operation DurationSign takes argument duration (a ~~Duration Record~~ Temporal.Duration) and returns one of -1, 0, or 1. It returns 1 if the most significant non-zero field in the duration argument is positive, and -1 if the most significant non-zero field is negative. If all of duration's fields are zero, it returns 0. It performs the following steps when called:

1. For each value v of « duration.[[Years]], duration.[[Months]], duration.[[Weeks]], duration.[[Days]], duration.[[Hours]], duration.[[Minutes]], duration.[[Seconds]], duration.[[Milliseconds]], duration.[[Microseconds]], duration.[[Nanoseconds]] », do
   1. If v < 0, return -1.
   2. If v > 0, return 1.
2. Return 0.

# 7.5.14 DateDurationSign ( dateDuration )

The abstract operation DateDurationSign takes argument dateDuration (a Date Duration Record) and returns one of -1, 0, or 1. It returns 1 if the most significant non-zero field in the dateDuration argument is positive, and -1 if the most significant non-zero field is negative. If all of dateDuration's fields are zero, it returns 0. It performs the following steps when called:

1. For each value v of « dateDuration.[[Years]], dateDuration.[[Months]], dateDuration.[[Weeks]], dateDuration.[[Days]] », do
   1. If v < 0, return -1.
   2. If v > 0, return 1.
2. Return 0.

# 7.5.15 InternalDurationSign ( internalDuration )

The abstract operation InternalDurationSign takes argument internalDuration (an Internal Duration Record) and returns one of -1, 0, or 1. It returns 1 if the most significant non-zero field in the internalDuration argument is positive, and -1 if the most significant non-zero field is negative. If all of internalDuration's fields are zero, it returns 0. It performs the following steps when called:

1. Let dateSign be DateDurationSign(internalDuration.[[Date]]).
2. If dateSign ≠ 0, return dateSign.
3. Return TimeDurationSign(internalDuration.[[Time]]).

# 7.5.16 IsValidDuration ( years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds )

The abstract operation IsValidDuration takes arguments years (an integer), months (an integer), weeks (an integer), days (an integer), hours (an integer), minutes (an integer), seconds (an integer), milliseconds (an integer), microseconds (an integer), and nanoseconds (an integer) and returns a Boolean. It returns true if its arguments form valid input from which to construct a ~~Duration Record~~ Temporal.Duration, and false otherwise. It performs the following steps when called:

1. Let sign be 0.
2. For each value v of « years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds », do
   1. Assert: 𝔽(v) is finite.
   2. If v < 0, then
      1. If sign > 0, return false.
      2. Set sign to -1.
   3. Else if v > 0, then
      1. If sign < 0, return false.
      2. Set sign to 1.
3. If abs(years) ≥ 232, return false.
4. If abs(months) ≥ 232, return false.
5. If abs(weeks) ≥ 232, return false.
6. Let normalizedNanoseconds be days × 86,400 × 109 \+ hours × 3600 × 109 \+ minutes × 60 × 109 \+ seconds × 109 \+ ℝ(𝔽(milliseconds)) × 106 \+ ℝ(𝔽(microseconds)) × 103 \+ ℝ(𝔽(nanoseconds)).
7. NOTE: The above step cannot be implemented directly using 64-bit floating-point arithmetic. Multiplying by 10-3, 10-6, and 10-9 respectively may be imprecise when milliseconds, microseconds, or nanoseconds is an unsafe integer. The step can be implemented by using 128-bit integers and performing all arithmetic on nanosecond values. It could also be implemented in C++ with an implementation of `std::remquo()` with sufficient bits in the quotient. String manipulation will also give an exact result, since the multiplication is by a power of 10.
8. If abs(normalizedNanoseconds) ≥ 109 × 253, return false.
9. Return true.

# 7.5.17 DefaultTemporalLargestUnit ( duration )

The abstract operation DefaultTemporalLargestUnit takes argument duration (a Temporal.Duration) and returns a Temporal unit. It implements the logic used in the `Temporal.Duration.prototype.round()` method and elsewhere, where the `largestUnit` option, if not given explicitly, is set to the largest-magnitude non-zero unit. It performs the following steps when called:

1. If duration.[[Years]] ≠ 0, return year.
2. If duration.[[Months]] ≠ 0, return month.
3. If duration.[[Weeks]] ≠ 0, return week.
4. If duration.[[Days]] ≠ 0, return day.
5. If duration.[[Hours]] ≠ 0, return hour.
6. If duration.[[Minutes]] ≠ 0, return minute.
7. If duration.[[Seconds]] ≠ 0, return second.
8. If duration.[[Milliseconds]] ≠ 0, return millisecond.
9. If duration.[[Microseconds]] ≠ 0, return microsecond.
10. Return nanosecond.

# 7.5.18 ToTemporalPartialDurationRecord ( temporalDurationLike )

The abstract operation ToTemporalPartialDurationRecord takes argument temporalDurationLike (an ECMAScript language value) and returns either a normal completion containing a partial Duration Record or a throw completion. The returned Record has its fields set according to the properties of temporalDurationLike. It performs the following steps when called:

1. If temporalDurationLike is not an Object, throw a TypeError exception.
2. Let result be a new partial Duration Record with each field set to undefined.
3. NOTE: The following steps read properties and perform independent validation in alphabetical order.
4. Let days be ? Get(temporalDurationLike, "days").
5. If days is not undefined, set result.[[Days]] to ? ToIntegerIfIntegral(days).
6. Let hours be ? Get(temporalDurationLike, "hours").
7. If hours is not undefined, set result.[[Hours]] to ? ToIntegerIfIntegral(hours).
8. Let microseconds be ? Get(temporalDurationLike, "microseconds").
9. If microseconds is not undefined, set result.[[Microseconds]] to ? ToIntegerIfIntegral(microseconds).
10. Let milliseconds be ? Get(temporalDurationLike, "milliseconds").
11. If milliseconds is not undefined, set result.[[Milliseconds]] to ? ToIntegerIfIntegral(milliseconds).
12. Let minutes be ? Get(temporalDurationLike, "minutes").
13. If minutes is not undefined, set result.[[Minutes]] to ? ToIntegerIfIntegral(minutes).
14. Let months be ? Get(temporalDurationLike, "months").
15. If months is not undefined, set result.[[Months]] to ? ToIntegerIfIntegral(months).
16. Let nanoseconds be ? Get(temporalDurationLike, "nanoseconds").
17. If nanoseconds is not undefined, set result.[[Nanoseconds]] to ? ToIntegerIfIntegral(nanoseconds).
18. Let seconds be ? Get(temporalDurationLike, "seconds").
19. If seconds is not undefined, set result.[[Seconds]] to ? ToIntegerIfIntegral(seconds).
20. Let weeks be ? Get(temporalDurationLike, "weeks").
21. If weeks is not undefined, set result.[[Weeks]] to ? ToIntegerIfIntegral(weeks).
22. Let years be ? Get(temporalDurationLike, "years").
23. If years is not undefined, set result.[[Years]] to ? ToIntegerIfIntegral(years).
24. If years is undefined, and months is undefined, and weeks is undefined, and days is undefined, and hours is undefined, and minutes is undefined, and seconds is undefined, and milliseconds is undefined, and microseconds is undefined, and nanoseconds is undefined, throw a TypeError exception.
25. Return result.

# 7.5.19 CreateTemporalDuration ( years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds [ , newTarget ] )

The abstract operation CreateTemporalDuration takes arguments years (an integer), months (an integer), weeks (an integer), days (an integer), hours (an integer), minutes (an integer), seconds (an integer), milliseconds (an integer), microseconds (an integer), and nanoseconds (an integer) and optional argument newTarget (a constructor) and returns either a normal completion containing a Temporal.Duration or a throw completion. It creates a Temporal.Duration instance and fills the internal slots with valid values. It performs the following steps when called:

1. If IsValidDuration(years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds) is false, throw a RangeError exception.
2. If newTarget is not present, set newTarget to %Temporal.Duration%.
3. Let object be ? OrdinaryCreateFromConstructor(newTarget, "%Temporal.Duration.prototype%", « [[InitializedTemporalDuration]], [[Years]], [[Months]], [[Weeks]], [[Days]], [[Hours]], [[Minutes]], [[Seconds]], [[Milliseconds]], [[Microseconds]], [[Nanoseconds]] »).
4. Set object.[[Years]] to ℝ(𝔽(years)).
5. Set object.[[Months]] to ℝ(𝔽(months)).
6. Set object.[[Weeks]] to ℝ(𝔽(weeks)).
7. Set object.[[Days]] to ℝ(𝔽(days)).
8. Set object.[[Hours]] to ℝ(𝔽(hours)).
9. Set object.[[Minutes]] to ℝ(𝔽(minutes)).
10. Set object.[[Seconds]] to ℝ(𝔽(seconds)).
11. Set object.[[Milliseconds]] to ℝ(𝔽(milliseconds)).
12. Set object.[[Microseconds]] to ℝ(𝔽(microseconds)).
13. Set object.[[Nanoseconds]] to ℝ(𝔽(nanoseconds)).
14. Return object.

# 7.5.20 CreateNegatedTemporalDuration ( duration )

The abstract operation CreateNegatedTemporalDuration takes argument duration (a Temporal.Duration) and returns a Temporal.Duration. It returns a new Temporal.Duration instance that is the negation of duration. It performs the following steps when called:

1. Return ! CreateTemporalDuration(-duration.[[Years]], -duration.[[Months]], -duration.[[Weeks]], -duration.[[Days]], -duration.[[Hours]], -duration.[[Minutes]], -duration.[[Seconds]], -duration.[[Milliseconds]], -duration.[[Microseconds]], -duration.[[Nanoseconds]]).

# 7.5.21 TimeDurationFromComponents ( hours, minutes, seconds, milliseconds, microseconds, nanoseconds )

The abstract operation TimeDurationFromComponents takes arguments hours (an integer), minutes (an integer), seconds (an integer), milliseconds (an integer), microseconds (an integer), and nanoseconds (an integer) and returns a time duration. From the given units, it computes a time duration consisting of total nanoseconds. The time duration can be stored losslessly in two 64-bit floating point numbers consisting of truncate(nanoseconds / 109) and remainder(nanoseconds, 109). Alternatively, nanoseconds can be stored as a 96-bit integer. It performs the following steps when called:

1. Set minutes to minutes \+ hours × 60.
2. Set seconds to seconds \+ minutes × 60.
3. Set milliseconds to milliseconds \+ seconds × 1000.
4. Set microseconds to microseconds \+ milliseconds × 1000.
5. Set nanoseconds to nanoseconds \+ microseconds × 1000.
6. Assert: abs(nanoseconds) ≤ maxTimeDuration.
7. Return nanoseconds.

# 7.5.22 AddTimeDuration ( one, two )

The abstract operation AddTimeDuration takes arguments one (a time duration) and two (a time duration) and returns either a normal completion containing a time duration or a throw completion. It returns a time duration that is the sum of one and two, throwing an exception if the result is greater than the maximum time duration. It performs the following steps when called:

1. Let result be one \+ two.
2. If abs(result) > maxTimeDuration, throw a RangeError exception.
3. Return result.

# 7.5.23 Add24HourDaysToTimeDuration ( d, days )

The abstract operation Add24HourDaysToTimeDuration takes arguments d (a time duration) and days (an integer) and returns either a normal completion containing a time duration or a throw completion. It returns a time duration that is the sum of d and the number of 24-hour days indicated by days, throwing an exception if the result is greater than the maximum time duration. This operation should not be used when adding days relative to a Temporal.ZonedDateTime, since the days may not be 24 hours long. It performs the following steps when called:

1. Let result be d \+ days × nsPerDay.
2. If abs(result) > maxTimeDuration, throw a RangeError exception.
3. Return result.

# 7.5.24 AddTimeDurationToEpochNanoseconds ( d, epochNs )

The abstract operation AddTimeDurationToEpochNanoseconds takes arguments d (a time duration) and epochNs (a BigInt) and returns a BigInt. It adds a time duration d to an exact time in nanoseconds since the epoch, epochNs, and returns a new exact time. The returned exact time is not required to be valid according to IsValidEpochNanoseconds. It performs the following steps when called:

1. Return epochNs \+ ℤ(d).

# 7.5.25 CompareTimeDuration ( one, two )

The abstract operation CompareTimeDuration takes arguments one (a time duration) and two (a time duration) and returns one of -1, 0, or 1. It performs a comparison of two time durations. It performs the following steps when called:

1. If one > two, return 1.
2. If one < two, return -1.
3. Return 0.

# 7.5.26 TimeDurationFromEpochNanosecondsDifference ( one, two )

The abstract operation TimeDurationFromEpochNanosecondsDifference takes arguments one (a BigInt) and two (a BigInt) and returns a time duration. The returned time duration is the difference between two exact times in nanoseconds since the epoch, which must not be greater than the maximum time duration. It performs the following steps when called:

1. Let result be ℝ(one) - ℝ(two).
2. Assert: abs(result) ≤ maxTimeDuration.
3. Return result.

# 7.5.27 RoundTimeDurationToIncrement ( d, increment, roundingMode )

The abstract operation RoundTimeDurationToIncrement takes arguments d (a time duration), increment (a positive integer), and roundingMode (a rounding mode) and returns either a normal completion containing a time duration or a throw completion. It rounds the total number of nanoseconds in the time duration d to the nearest multiple of increment, up or down according to roundingMode. It performs the following steps when called:

1. Let rounded be RoundNumberToIncrement(d, increment, roundingMode).
2. If abs(rounded) > maxTimeDuration, throw a RangeError exception.
3. Return rounded.

# 7.5.28 TimeDurationSign ( d )

The abstract operation TimeDurationSign takes argument d (a time duration) and returns one of -1, 0, or 1. It returns 0 if the duration is zero, or ±1 depending on the sign of the duration. It performs the following steps when called:

1. If d < 0, return -1.
2. If d > 0, return 1.
3. Return 0.

# 7.5.29 DateDurationDays ( dateDuration, plainRelativeTo )

The abstract operation DateDurationDays takes arguments dateDuration (a Date Duration Record) and plainRelativeTo (a Temporal.PlainDate) and returns either a normal completion containing an integer or a throw completion. It converts the calendar units of a duration into a number of days, and returns the result. It performs the following steps when called:

1. Let yearsMonthsWeeksDuration be ! AdjustDateDurationRecord(dateDuration, 0).
2. If DateDurationSign(yearsMonthsWeeksDuration) = 0, return dateDuration.[[Days]].
3. Let later be ? CalendarDateAdd(plainRelativeTo.[[Calendar]], plainRelativeTo.[[ISODate]], yearsMonthsWeeksDuration, constrain).
4. Let epochDays1 be ISODateToEpochDays(plainRelativeTo.[[ISODate]].[[Year]], plainRelativeTo.[[ISODate]].[[Month]] \- 1, plainRelativeTo.[[ISODate]].[[Day]]).
5. Let epochDays2 be ISODateToEpochDays(later.[[Year]], later.[[Month]] \- 1, later.[[Day]]).
6. Let yearsMonthsWeeksInDays be epochDays2 \- epochDays1.
7. Return dateDuration.[[Days]] \+ yearsMonthsWeeksInDays.

# 7.5.30 RoundTimeDuration ( timeDuration, increment, unit, roundingMode )

The abstract operation RoundTimeDuration takes arguments timeDuration (a time duration), increment (a positive integer), unit (a time unit), and roundingMode (a rounding mode) and returns either a normal completion containing a time duration, or a throw completion. It rounds a timeDuration according to the rounding parameters unit, increment, and roundingMode, and returns the time duration result. It performs the following steps when called:

1. Let divisor be the value in the "Length in Nanoseconds" column of the row of Table 21 whose "Value" column contains unit.
2. Return ? RoundTimeDurationToIncrement(timeDuration, divisor × increment, roundingMode).

# 7.5.31 TotalTimeDuration ( timeDuration, unit )

The abstract operation TotalTimeDuration takes arguments timeDuration (a time duration) and unit (either a time unit or day) and returns a mathematical value. It returns the total number of unit in duration. It performs the following steps when called:

1. Let divisor be the value in the "Length in Nanoseconds" column of the row of Table 21 whose "Value" column contains unit.
2. NOTE: The following step cannot be implemented directly using floating-point arithmetic when 𝔽(timeDuration) is not a safe integer. The division can be implemented in C++ with the `__float128` type if the compiler supports it, or with software emulation such as in the SoftFP library.
3. Return timeDuration / divisor.

# 7.5.32 Duration Nudge Result Records

A Duration Nudge Result Record is a Record value used to represent the result of rounding a duration up or down to an increment relative to a date-time, as in NudgeToCalendarUnit, NudgeToZonedTime, or NudgeToDayOrTime.

Duration Nudge Result Records have the fields listed in Table 13.

| Table 13: Duration Nudge Result Record Fields Field Name | Value                       | Meaning                                                                                      |
| -------------------------------------------------------- | --------------------------- | -------------------------------------------------------------------------------------------- |
| [[Duration]]                                             | an Internal Duration Record | The resulting duration.                                                                      |
| [[NudgedEpochNs]]                                        | a BigInt                    | The epoch time corresponding to the rounded duration, relative to the starting point.        |
| [[DidExpandCalendarUnit]]                                | a Boolean                   | Whether the rounding operation caused the duration to expand to the next day or larger unit. |

# 7.5.33 ComputeNudgeWindow ( sign, duration, originEpochNs, isoDateTime, timeZone, calendar, increment, unit, additionalShift )

The abstract operation ComputeNudgeWindow takes arguments sign (either -1 or 1), duration (an Internal Duration Record), originEpochNs (a BigInt), isoDateTime (an ISO Date-Time Record), timeZone (either an available time zone identifier or unset), calendar (a calendar type), increment (a positive integer), unit (a date unit), and additionalShift (a Boolean) and returns either a normal completion containing a Record with fields [[R1]] (a mathematical value), [[R2]] (a mathematical value), [[StartEpochNs]] (a BigInt), [[EndEpochNs]] (a BigInt), [[StartDuration]] (an Internal Duration Record), and [[EndDuration]] (an Internal Duration Record), or a throw completion. It implements calculating the upper and lower bounds of the starting point added to the duration in epoch nanoseconds. It performs the following steps when called:

1. If unit is year, then
   1. Let years be RoundNumberToIncrement(duration.[[Date]].[[Years]], increment, trunc).
   2. If additionalShift is false, then
      1. Let r1 be years.
   3. Else,
      1. Let r1 be years \+ increment × sign.
   4. Let r2 be r1 \+ increment × sign.
   5. Let startDateDuration be ? CreateDateDurationRecord(r1, 0, 0, 0).
   6. Let endDateDuration be ? CreateDateDurationRecord(r2, 0, 0, 0).
2. Else if unit is month, then
   1. Let months be RoundNumberToIncrement(duration.[[Date]].[[Months]], increment, trunc).
   2. If additionalShift is false, then
      1. Let r1 be months.
   3. Else,
      1. Let r1 be months \+ increment × sign.
   4. Let r2 be r1 \+ increment × sign.
   5. Let startDateDuration be ? AdjustDateDurationRecord(duration.[[Date]], 0, 0, r1).
   6. Let endDateDuration be ? AdjustDateDurationRecord(duration.[[Date]], 0, 0, r2).
3. Else if unit is week, then
   1. Let yearsMonths be ! AdjustDateDurationRecord(duration.[[Date]], 0, 0).
   2. Let weeksStart be ? CalendarDateAdd(calendar, isoDateTime.[[ISODate]], yearsMonths, constrain).
   3. Let weeksEnd be AddDaysToISODate(weeksStart, duration.[[Date]].[[Days]]).
   4. Let untilResult be CalendarDateUntil(calendar, weeksStart, weeksEnd, week).
   5. Let weeks be RoundNumberToIncrement(duration.[[Date]].[[Weeks]] \+ untilResult.[[Weeks]], increment, trunc).
   6. Let r1 be weeks.
   7. Let r2 be weeks \+ increment × sign.
   8. Let startDateDuration be ? AdjustDateDurationRecord(duration.[[Date]], 0, r1).
   9. Let endDateDuration be ? AdjustDateDurationRecord(duration.[[Date]], 0, r2).
4. Else,
   1. Assert: unit is day.
   2. Let days be RoundNumberToIncrement(duration.[[Date]].[[Days]], increment, trunc).
   3. Let r1 be days.
   4. Let r2 be days \+ increment × sign.
   5. Let startDateDuration be ? AdjustDateDurationRecord(duration.[[Date]], r1).
   6. Let endDateDuration be ? AdjustDateDurationRecord(duration.[[Date]], r2).
5. Assert: If sign = 1, r1 ≥ 0 and r1 < r2.
6. Assert: If sign = -1, r1 ≤ 0 and r1 > r2.
7. If r1 = 0, then
   1. Let startEpochNs be originEpochNs.
8. Else,
   1. Let start be ? CalendarDateAdd(calendar, isoDateTime.[[ISODate]], startDateDuration, constrain).
   2. Let startDateTime be CombineISODateAndTimeRecord(start, isoDateTime.[[Time]]).
   3. If timeZone is unset, then
      1. Let startEpochNs be GetUTCEpochNanoseconds(startDateTime).
   4. Else,
      1. Let startEpochNs be ? GetEpochNanosecondsFor(timeZone, startDateTime, compatible).
9. Let end be ? CalendarDateAdd(calendar, isoDateTime.[[ISODate]], endDateDuration, constrain).
10. Let endDateTime be CombineISODateAndTimeRecord(end, isoDateTime.[[Time]]).
11. If timeZone is unset, then
    1. Let endEpochNs be GetUTCEpochNanoseconds(endDateTime).
12. Else,
    1. Let endEpochNs be ? GetEpochNanosecondsFor(timeZone, endDateTime, compatible).
13. Let startDuration be CombineDateAndTimeDuration(startDateDuration, 0).
14. Let endDuration be CombineDateAndTimeDuration(endDateDuration, 0).
15. Return the Record { [[R1]]: r1, [[R2]]: r2, [[StartEpochNs]]: startEpochNs, [[EndEpochNs]]: endEpochNs, [[StartDuration]]: startDuration, [[EndDuration]]: endDuration }.

# 7.5.34 NudgeToCalendarUnit ( sign, duration, originEpochNs, destEpochNs, isoDateTime, timeZone, calendar, increment, unit, roundingMode )

The abstract operation NudgeToCalendarUnit takes arguments sign (either -1 or 1), duration (an Internal Duration Record), originEpochNs (a BigInt), destEpochNs (a BigInt), isoDateTime (an ISO Date-Time Record), timeZone (either an available time zone identifier or unset), calendar (a calendar type), increment (a positive integer), unit (a date unit), and roundingMode (a rounding mode) and returns either a normal completion containing a Record with fields [[NudgeResult]] (a Duration Nudge Result Record) and [[Total]] (a mathematical value), or a throw completion. It implements rounding a duration to an increment of a calendar unit, relative to a starting point, by calculating the upper and lower bounds of the starting point added to the duration in epoch nanoseconds, and rounding according to which one is closer to destEpochNs. It performs the following steps when called:

1. Let didExpandCalendarUnit be false.
2. Let nudgeWindow be ? ComputeNudgeWindow(sign, duration, originEpochNs, isoDateTime, timeZone, calendar, increment, unit, false).
3. Let startEpochNs be nudgeWindow.[[StartEpochNs]].
4. Let endEpochNs be nudgeWindow.[[EndEpochNs]].
5. If sign = 1, then
   1. If startEpochNs ≤ destEpochNs ≤ endEpochNs is false, then
      1. Set nudgeWindow to ? ComputeNudgeWindow(sign, duration, originEpochNs, isoDateTime, timeZone, calendar, increment, unit, true).
      2. Assert: nudgeWindow.[[StartEpochNs]] ≤ destEpochNs ≤ nudgeWindow.[[EndEpochNs]].
      3. Set didExpandCalendarUnit to true.
6. Else,
   1. If endEpochNs ≤ destEpochNs ≤ startEpochNs is false, then
      1. Set nudgeWindow to ? ComputeNudgeWindow(sign, duration, originEpochNs, isoDateTime, timeZone, calendar, increment, unit, true).
      2. Assert: nudgeWindow.[[EndEpochNs]] ≤ destEpochNs ≤ nudgeWindow.[[StartEpochNs]].
      3. Set didExpandCalendarUnit to true.
7. Let r1 be nudgeWindow.[[R1]].
8. Let r2 be nudgeWindow.[[R2]].
9. Set startEpochNs to nudgeWindow.[[StartEpochNs]].
10. Set endEpochNs to nudgeWindow.[[EndEpochNs]].
11. Let startDuration be nudgeWindow.[[StartDuration]].
12. Let endDuration be nudgeWindow.[[EndDuration]].
13. Assert: startEpochNs ≠ endEpochNs.
14. Let progress be (destEpochNs \- startEpochNs) / (endEpochNs \- startEpochNs).
15. Let total be r1 \+ progress × increment × sign.
16. NOTE: The above two steps cannot be implemented directly using floating-point arithmetic. This division can be implemented as if expressing total as the quotient of two time durations (which may not be safe integers), performing all other calculations before the division, and finally performing one division operation with a floating-point result for total. The division can be implemented in C++ with the `__float128` type if the compiler supports it, or with software emulation such as in the SoftFP library.
17. Assert: 0 ≤ progress ≤ 1.
18. If sign < 0, let isNegative be negative; else let isNegative be positive.
19. Let unsignedRoundingMode be GetUnsignedRoundingMode(roundingMode, isNegative).
20. If progress = 1, then
    1. Let roundedUnit be abs(r2).
21. Else,
    1. Assert: abs(r1) ≤ abs(total) < abs(r2).
    2. Let roundedUnit be ApplyUnsignedRoundingMode(abs(total), abs(r1), abs(r2), unsignedRoundingMode).
22. If roundedUnit is abs(r2), then
    1. Set didExpandCalendarUnit to true.
    2. Let resultDuration be endDuration.
    3. Let nudgedEpochNs be endEpochNs.
23. Else,
    1. Let resultDuration be startDuration.
    2. Let nudgedEpochNs be startEpochNs.
24. Let nudgeResult be Duration Nudge Result Record { [[Duration]]: resultDuration, [[NudgedEpochNs]]: nudgedEpochNs, [[DidExpandCalendarUnit]]: didExpandCalendarUnit }.
25. Return the Record { [[NudgeResult]]: nudgeResult, [[Total]]: total }.

# 7.5.35 NudgeToZonedTime ( sign, duration, isoDateTime, timeZone, calendar, increment, unit, roundingMode )

The abstract operation NudgeToZonedTime takes arguments sign (either -1 or 1), duration (an Internal Duration Record), isoDateTime (an ISO Date-Time Record), timeZone (an available time zone identifier), calendar (a calendar type), increment (a positive integer), unit (a time unit), and roundingMode (a rounding mode) and returns either a normal completion containing a Duration Nudge Result Record or a throw completion. It implements rounding a duration to an increment of a time unit, relative to a `Temporal.ZonedDateTime` starting point, accounting for the case where the rounding causes the time to exceed the total time within a day, which may be influenced by UTC offset changes in the time zone. It performs the following steps when called:

1. Let start be ? CalendarDateAdd(calendar, isoDateTime.[[ISODate]], duration.[[Date]], constrain).
2. Let startDateTime be CombineISODateAndTimeRecord(start, isoDateTime.[[Time]]).
3. Let endDate be AddDaysToISODate(start, sign).
4. Let endDateTime be CombineISODateAndTimeRecord(endDate, isoDateTime.[[Time]]).
5. Let startEpochNs be ? GetEpochNanosecondsFor(timeZone, startDateTime, compatible).
6. Let endEpochNs be ? GetEpochNanosecondsFor(timeZone, endDateTime, compatible).
7. Let daySpan be TimeDurationFromEpochNanosecondsDifference(endEpochNs, startEpochNs).
8. Assert: TimeDurationSign(daySpan) = sign.
9. Let unitLength be the value in the "Length in Nanoseconds" column of the row of Table 21 whose "Value" column contains unit.
10. Let roundedTimeDuration be ? RoundTimeDurationToIncrement(duration.[[Time]], increment × unitLength, roundingMode).
11. Let beyondDaySpan be ! AddTimeDuration(roundedTimeDuration, -daySpan).
12. If TimeDurationSign(beyondDaySpan) ≠ -sign, then
    1. Let didRoundBeyondDay be true.
    2. Let dayDelta be sign.
    3. Set roundedTimeDuration to ? RoundTimeDurationToIncrement(beyondDaySpan, increment × unitLength, roundingMode).
    4. Let nudgedEpochNs be AddTimeDurationToEpochNanoseconds(roundedTimeDuration, endEpochNs).
13. Else,
    1. Let didRoundBeyondDay be false.
    2. Let dayDelta be 0.
    3. Let nudgedEpochNs be AddTimeDurationToEpochNanoseconds(roundedTimeDuration, startEpochNs).
14. Let dateDuration be ! AdjustDateDurationRecord(duration.[[Date]], duration.[[Date]].[[Days]] \+ dayDelta).
15. Let resultDuration be CombineDateAndTimeDuration(dateDuration, roundedTimeDuration).
16. Return Duration Nudge Result Record { [[Duration]]: resultDuration, [[NudgedEpochNs]]: nudgedEpochNs, [[DidExpandCalendarUnit]]: didRoundBeyondDay }.

# 7.5.36 NudgeToDayOrTime ( duration, destEpochNs, largestUnit, increment, smallestUnit, roundingMode )

The abstract operation NudgeToDayOrTime takes arguments duration (an Internal Duration Record), destEpochNs (a BigInt), largestUnit (a Temporal unit), increment (a positive integer), smallestUnit (either a time unit or day), and roundingMode (a rounding mode) and returns either a normal completion containing a Duration Nudge Result Record or a throw completion. It implements rounding a duration to an increment of a time unit, in cases unaffected by the calendar or time zone. It performs the following steps when called:

1. Let timeDuration be ! Add24HourDaysToTimeDuration(duration.[[Time]], duration.[[Date]].[[Days]]).
2. Let unitLength be the value in the "Length in Nanoseconds" column of the row of Table 21 whose "Value" column contains smallestUnit.
3. Let roundedTime be ? RoundTimeDurationToIncrement(timeDuration, unitLength × increment, roundingMode).
4. Let diffTime be ! AddTimeDuration(roundedTime, -timeDuration).
5. Let wholeDays be truncate(TotalTimeDuration(timeDuration, day)).
6. Let roundedWholeDays be truncate(TotalTimeDuration(roundedTime, day)).
7. Let dayDelta be roundedWholeDays \- wholeDays.
8. If dayDelta < 0, let dayDeltaSign be -1; else if dayDelta > 0, let dayDeltaSign be 1; else let dayDeltaSign be 0.
9. If dayDeltaSign = TimeDurationSign(timeDuration), let didExpandDays be true; else let didExpandDays be false.
10. Let nudgedEpochNs be AddTimeDurationToEpochNanoseconds(diffTime, destEpochNs).
11. Let days be 0.
12. Let remainder be roundedTime.
13. If TemporalUnitCategory(largestUnit) is date, then
    1. Set days to roundedWholeDays.
    2. Set remainder to ! AddTimeDuration(roundedTime, TimeDurationFromComponents(-roundedWholeDays * HoursPerDay, 0, 0, 0, 0, 0)).
14. Let dateDuration be ! AdjustDateDurationRecord(duration.[[Date]], days).
15. Let resultDuration be CombineDateAndTimeDuration(dateDuration, remainder).
16. Return Duration Nudge Result Record { [[Duration]]: resultDuration, [[NudgedEpochNs]]: nudgedEpochNs, [[DidExpandCalendarUnit]]: didExpandDays }.

# 7.5.37 BubbleRelativeDuration ( sign, duration, nudgedEpochNs, isoDateTime, timeZone, calendar, largestUnit, smallestUnit )

The abstract operation BubbleRelativeDuration takes arguments sign (either -1 or 1), duration (an Internal Duration Record), nudgedEpochNs (a BigInt), isoDateTime (an ISO Date-Time Record), timeZone (either an available time zone identifier or unset), calendar (a calendar type), largestUnit (a Temporal unit), and smallestUnit (a date unit) and returns either a normal completion containing an Internal Duration Record or a throw completion. Given a duration that has potentially been made bottom-heavy by rounding in NudgeToCalendarUnit, NudgeToZonedTime, or NudgeToDayOrTime, it bubbles up smaller units to larger units. It performs the following steps when called:

1. If smallestUnit is largestUnit, return duration.
2. Let largestUnitIndex be the ordinal index of the row of Table 21 whose "Value" column contains largestUnit.
3. Let smallestUnitIndex be the ordinal index of the row of Table 21 whose "Value" column contains smallestUnit.
4. Let unitIndex be smallestUnitIndex \- 1.
5. Let done be false.
6. Repeat, while unitIndex ≥ largestUnitIndex and done is false,
   1. Let unit be the value in the "Value" column of Table 21 in the row whose ordinal index is unitIndex.
   2. If unit is not week, or largestUnit is week, then
      1. If unit is year, then
         1. Let years be duration.[[Date]].[[Years]] \+ sign.
         2. Let endDuration be ? CreateDateDurationRecord(years, 0, 0, 0).
      2. Else if unit is month, then
         1. Let months be duration.[[Date]].[[Months]] \+ sign.
         2. Let endDuration be ? AdjustDateDurationRecord(duration.[[Date]], 0, 0, months).
      3. Else,
         1. Assert: unit is week.
         2. Let weeks be duration.[[Date]].[[Weeks]] \+ sign.
         3. Let endDuration be ? AdjustDateDurationRecord(duration.[[Date]], 0, weeks).
      4. Let end be ? CalendarDateAdd(calendar, isoDateTime.[[ISODate]], endDuration, constrain).
      5. Let endDateTime be CombineISODateAndTimeRecord(end, isoDateTime.[[Time]]).
      6. If timeZone is unset, then
         1. Let endEpochNs be GetUTCEpochNanoseconds(endDateTime).
      7. Else,
         1. Let endEpochNs be ? GetEpochNanosecondsFor(timeZone, endDateTime, compatible).
      8. Let beyondEnd be nudgedEpochNs \- endEpochNs.
      9. If beyondEnd < 0, let beyondEndSign be -1; else if beyondEnd > 0, let beyondEndSign be 1; else let beyondEndSign be 0.
      10. If beyondEndSign ≠ -sign, then
      11. Set duration to CombineDateAndTimeDuration(endDuration, 0).
      12. Else,
      13. Set done to true.
   3. Set unitIndex to unitIndex \- 1.
7. Return duration.

# 7.5.38 RoundRelativeDuration ( duration, originEpochNs, destEpochNs, isoDateTime, timeZone, calendar, largestUnit, increment, smallestUnit, roundingMode )

The abstract operation RoundRelativeDuration takes arguments duration (an Internal Duration Record), originEpochNs (a BigInt), destEpochNs (a BigInt), isoDateTime (an ISO Date-Time Record), timeZone (either an available time zone identifier or unset), calendar (a calendar type), largestUnit (a Temporal unit), increment (a positive integer), smallestUnit (a Temporal unit), and roundingMode (a rounding mode) and returns either a normal completion containing an Internal Duration Record or a throw completion. It rounds a duration duration relative to isoDateTime according to the rounding parameters smallestUnit, increment, and roundingMode, bubbles overflows up to the next highest unit until largestUnit, and returns the rounded duration. It performs the following steps when called:

1. Let irregularLengthUnit be false.
2. If IsCalendarUnit(smallestUnit) is true, set irregularLengthUnit to true.
3. If timeZone is not unset and smallestUnit is day, set irregularLengthUnit to true.
4. If InternalDurationSign(duration) < 0, let sign be -1; else let sign be 1.
5. If irregularLengthUnit is true, then
   1. Let record be ? NudgeToCalendarUnit(sign, duration, originEpochNs, destEpochNs, isoDateTime, timeZone, calendar, increment, smallestUnit, roundingMode).
   2. Let nudgeResult be record.[[NudgeResult]].
6. Else if timeZone is not unset, then
   1. Let nudgeResult be ? NudgeToZonedTime(sign, duration, isoDateTime, timeZone, calendar, increment, smallestUnit, roundingMode).
7. Else,
   1. Let nudgeResult be ? NudgeToDayOrTime(duration, destEpochNs, largestUnit, increment, smallestUnit, roundingMode).
8. Set duration to nudgeResult.[[Duration]].
9. If nudgeResult.[[DidExpandCalendarUnit]] is true and smallestUnit is not week, then
   1. Let startUnit be LargerOfTwoTemporalUnits(smallestUnit, day).
   2. Set duration to ? BubbleRelativeDuration(sign, duration, nudgeResult.[[NudgedEpochNs]], isoDateTime, timeZone, calendar, largestUnit, startUnit).
10. Return duration.

# 7.5.39 TotalRelativeDuration ( duration, originEpochNs, destEpochNs, isoDateTime, timeZone, calendar, unit )

The abstract operation TotalRelativeDuration takes arguments duration (an Internal Duration Record), originEpochNs (a BigInt), destEpochNs (a BigInt), isoDateTime (an ISO Date-Time Record), timeZone (either an available time zone identifier or unset), calendar (a calendar type), and unit (a Temporal unit) and returns either a normal completion containing a mathematical value, or a throw completion. It returns the total number of unit in duration, relative to isoDateTime if a starting point is necessary for calendar units. It performs the following steps when called:

1. If IsCalendarUnit(unit) is true, or timeZone is not unset and unit is day, then
   1. If InternalDurationSign(duration) < 0, let sign be -1; else let sign be 1.
   2. Let record be ? NudgeToCalendarUnit(sign, duration, originEpochNs, destEpochNs, isoDateTime, timeZone, calendar, 1, unit, trunc).
   3. Return record.[[Total]].
2. Let timeDuration be ! Add24HourDaysToTimeDuration(duration.[[Time]], duration.[[Date]].[[Days]]).
3. Return TotalTimeDuration(timeDuration, unit).

# 7.5.40 TemporalDurationToString ( duration, precision )

The abstract operation TemporalDurationToString takes arguments duration (a Temporal.Duration) and precision (either an integer between 0 and 9 inclusive or auto) and returns a String. It returns a String which is the ISO 8601 representation of duration, with the number of decimal places in the seconds value controlled by precision. It performs the following steps when called:

1. Let sign be DurationSign(duration).
2. Let datePart be the empty String.
3. If duration.[[Years]] ≠ 0, then
   1. Set datePart to the string concatenation of abs(duration.[[Years]]) formatted as a decimal number and the code unit 0x0059 (LATIN CAPITAL LETTER Y).
4. If duration.[[Months]] ≠ 0, then
   1. Set datePart to the string concatenation of datePart, abs(duration.[[Months]]) formatted as a decimal number, and the code unit 0x004D (LATIN CAPITAL LETTER M).
5. If duration.[[Weeks]] ≠ 0, then
   1. Set datePart to the string concatenation of datePart, abs(duration.[[Weeks]]) formatted as a decimal number, and the code unit 0x0057 (LATIN CAPITAL LETTER W).
6. If duration.[[Days]] ≠ 0, then
   1. Set datePart to the string concatenation of datePart, abs(duration.[[Days]]) formatted as a decimal number, and the code unit 0x0044 (LATIN CAPITAL LETTER D).
7. Let timePart be the empty String.
8. If duration.[[Hours]] ≠ 0, then
   1. Set timePart to the string concatenation of abs(duration.[[Hours]]) formatted as a decimal number and the code unit 0x0048 (LATIN CAPITAL LETTER H).
9. If duration.[[Minutes]] ≠ 0, then
   1. Set timePart to the string concatenation of timePart, abs(duration.[[Minutes]]) formatted as a decimal number, and the code unit 0x004D (LATIN CAPITAL LETTER M).
10. Let zeroMinutesAndHigher be false.
11. If DefaultTemporalLargestUnit(duration) is one of second, millisecond, microsecond, or nanosecond, set zeroMinutesAndHigher to true.
12. Let secondsDuration be TimeDurationFromComponents(0, 0, duration.[[Seconds]], duration.[[Milliseconds]], duration.[[Microseconds]], duration.[[Nanoseconds]]).
13. If secondsDuration ≠ 0, or zeroMinutesAndHigher is true, or precision is not auto, then
    1. Let secondsPart be abs(truncate(secondsDuration / 109)) formatted as a decimal number.
    2. Let subSecondsPart be FormatFractionalSeconds(abs(remainder(secondsDuration, 109)), precision).
    3. Set timePart to the string concatenation of timePart, secondsPart, subSecondsPart, and the code unit 0x0053 (LATIN CAPITAL LETTER S).
14. Let signPart be the code unit 0x002D (HYPHEN-MINUS) if sign < 0, and otherwise the empty String.
15. Let result be the string concatenation of signPart, the code unit 0x0050 (LATIN CAPITAL LETTER P) and datePart.
16. If timePart is not the empty String, then
    1. Set result to the string concatenation of result, the code unit 0x0054 (LATIN CAPITAL LETTER T), and timePart.
17. Return result.

# 7.5.41 AddDurations ( operation, duration, other )

The abstract operation AddDurations takes arguments operation (either add or subtract), duration (a Temporal.Duration), and other (an ECMAScript language value) and returns either a normal completion containing a Temporal.Duration or a throw completion. It adds or subtracts the components of a second duration other to or from those of a first duration duration, resulting in a longer or shorter duration, unless calendar calculations would be required, in which case it throws an exception. It balances the result, ensuring that no mixed signs remain. It performs the following steps when called:

1. Set other to ? ToTemporalDuration(other).
2. If operation is subtract, set other to CreateNegatedTemporalDuration(other).
3. Let largestUnit1 be DefaultTemporalLargestUnit(duration).
4. Let largestUnit2 be DefaultTemporalLargestUnit(other).
5. Let largestUnit be LargerOfTwoTemporalUnits(largestUnit1, largestUnit2).
6. If IsCalendarUnit(largestUnit) is true, throw a RangeError exception.
7. Let d1 be ToInternalDurationRecordWith24HourDays(duration).
8. Let d2 be ToInternalDurationRecordWith24HourDays(other).
9. Let timeResult be ? AddTimeDuration(d1.[[Time]], d2.[[Time]]).
10. Let result be CombineDateAndTimeDuration(ZeroDateDuration(), timeResult).
11. Return ? TemporalDurationFromInternal(result, largestUnit).

# 8 Temporal.Instant Objects

A Temporal.Instant object is an Object referencing a fixed point in time with nanoseconds precision.

# 8.1 The Temporal.Instant Constructor

The Temporal.Instant constructor:

- creates and initializes a new Temporal.Instant object when called as a constructor.
- is not intended to be called as a function and will throw an exception when called in that manner.
- may be used as the value of an `extends` clause of a class definition. Subclass constructors that intend to inherit the specified Temporal.Instant behaviour must include a super call to the %Temporal.Instant% constructor to create and initialize subclass instances with the necessary internal slots.

# 8.1.1 Temporal.Instant ( epochNanoseconds )

This function performs the following steps when called:

1. If NewTarget is undefined, throw a TypeError exception.
2. Let epochNanoseconds be ? ToBigInt(epochNanoseconds).
3. If IsValidEpochNanoseconds(epochNanoseconds) is false, throw a RangeError exception.
4. Return ? CreateTemporalInstant(epochNanoseconds, NewTarget).

# 8.2 Properties of the Temporal.Instant Constructor

The value of the [[Prototype]] internal slot of the Temporal.Instant constructor is the intrinsic object %Function.prototype%.

The Temporal.Instant constructor has the following properties:

# 8.2.1 Temporal.Instant.prototype

The initial value of `Temporal.Instant.prototype` is %Temporal.Instant.prototype%.

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: false }.

# 8.2.2 Temporal.Instant.from ( item )

This function performs the following steps when called:

1. Return ? ToTemporalInstant(item).

# 8.2.3 Temporal.Instant.fromEpochMilliseconds ( epochMilliseconds )

This function performs the following steps when called:

1. Set epochMilliseconds to ? ToNumber(epochMilliseconds).
2. Set epochMilliseconds to ? NumberToBigInt(epochMilliseconds).
3. Let epochNanoseconds be epochMilliseconds × ℤ(106).
4. If IsValidEpochNanoseconds(epochNanoseconds) is false, throw a RangeError exception.
5. Return ! CreateTemporalInstant(epochNanoseconds).

# 8.2.4 Temporal.Instant.fromEpochNanoseconds ( epochNanoseconds )

This function performs the following steps when called:

1. Set epochNanoseconds to ? ToBigInt(epochNanoseconds).
2. If IsValidEpochNanoseconds(epochNanoseconds) is false, throw a RangeError exception.
3. Return ! CreateTemporalInstant(epochNanoseconds).

# 8.2.5 Temporal.Instant.compare ( one, two )

This function performs the following steps when called:

1. Set one to ? ToTemporalInstant(one).
2. Set two to ? ToTemporalInstant(two).
3. Return 𝔽(CompareEpochNanoseconds(one.[[EpochNanoseconds]], two.[[EpochNanoseconds]])).

# 8.3 Properties of the Temporal.Instant Prototype Object

The Temporal.Instant prototype object

- is itself an ordinary object.
- is not a Temporal.Instant instance and does not have a [[InitializedTemporalInstant]] internal slot.
- has a [[Prototype]] internal slot whose value is %Object.prototype%.

# 8.3.1 Temporal.Instant.prototype.constructor

The initial value of `Temporal.Instant.prototype.constructor` is %Temporal.Instant%.

# 8.3.2 Temporal.Instant.prototype[ %Symbol.toStringTag% ]

The initial value of the %Symbol.toStringTag% property is the String "Temporal.Instant".

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }.

# 8.3.3 get Temporal.Instant.prototype.epochMilliseconds

`Temporal.Instant.prototype.epochMilliseconds` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Let ns be instant.[[EpochNanoseconds]].
4. Let ms be floor(ℝ(ns) / 106).
5. Return 𝔽(ms).

# 8.3.4 get Temporal.Instant.prototype.epochNanoseconds

`Temporal.Instant.prototype.epochNanoseconds` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Return instant.[[EpochNanoseconds]].

# 8.3.5 Temporal.Instant.prototype.add ( temporalDurationLike )

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Return ? AddDurationToInstant(add, instant, temporalDurationLike).

# 8.3.6 Temporal.Instant.prototype.subtract ( temporalDurationLike )

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Return ? AddDurationToInstant(subtract, instant, temporalDurationLike).

# 8.3.7 Temporal.Instant.prototype.until ( other [ , options ] )

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Return ? DifferenceTemporalInstant(until, instant, other, options).

# 8.3.8 Temporal.Instant.prototype.since ( other [ , options ] )

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Return ? DifferenceTemporalInstant(since, instant, other, options).

# 8.3.9 Temporal.Instant.prototype.round ( roundTo )

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. If roundTo is undefined, throw a TypeError exception.
4. If roundTo is a String, then
   1. Let paramString be roundTo.
   2. Set roundTo to OrdinaryObjectCreate(null).
   3. Perform ! CreateDataPropertyOrThrow(roundTo, "smallestUnit", paramString).
5. Else,
   1. Set roundTo to ? GetOptionsObject(roundTo).
6. NOTE: The following steps read options and perform independent validation in alphabetical order (GetRoundingIncrementOption reads "roundingIncrement" and GetRoundingModeOption reads "roundingMode").
7. Let roundingIncrement be ? GetRoundingIncrementOption(roundTo).
8. Let roundingMode be ? GetRoundingModeOption(roundTo, half-expand).
9. Let smallestUnit be ? GetTemporalUnitValuedOption(roundTo, "smallestUnit", required).
10. Perform ? ValidateTemporalUnitValue(smallestUnit, time).
11. If smallestUnit is hour, then
    1. Let maximum be HoursPerDay.
12. Else if smallestUnit is minute, then
    1. Let maximum be MinutesPerHour × HoursPerDay.
13. Else if smallestUnit is second, then
    1. Let maximum be SecondsPerMinute × MinutesPerHour × HoursPerDay.
14. Else if smallestUnit is millisecond, then
    1. Let maximum be ℝ(msPerDay).
15. Else if smallestUnit is microsecond, then
    1. Let maximum be 103 × ℝ(msPerDay).
16. Else,
    1. Assert: smallestUnit is nanosecond.
    2. Let maximum be nsPerDay.
17. Perform ? ValidateTemporalRoundingIncrement(roundingIncrement, maximum, true).
18. Let roundedNs be RoundTemporalInstant(instant.[[EpochNanoseconds]], roundingIncrement, smallestUnit, roundingMode).
19. Return ! CreateTemporalInstant(roundedNs).

# 8.3.10 Temporal.Instant.prototype.equals ( other )

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Set other to ? ToTemporalInstant(other).
4. If instant.[[EpochNanoseconds]] ≠ other.[[EpochNanoseconds]], return false.
5. Return true.

# 8.3.11 Temporal.Instant.prototype.toString ( [ options ] )

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. NOTE: The following steps read options and perform independent validation in alphabetical order (GetTemporalFractionalSecondDigitsOption reads "fractionalSecondDigits" and GetRoundingModeOption reads "roundingMode").
5. Let digits be ? GetTemporalFractionalSecondDigitsOption(resolvedOptions).
6. Let roundingMode be ? GetRoundingModeOption(resolvedOptions, trunc).
7. Let smallestUnit be ? GetTemporalUnitValuedOption(resolvedOptions, "smallestUnit", unset).
8. Let timeZone be ? Get(resolvedOptions, "timeZone").
9. Perform ? ValidateTemporalUnitValue(smallestUnit, time).
10. If smallestUnit is hour, throw a RangeError exception.
11. If timeZone is not undefined, then
    1. Set timeZone to ? ToTemporalTimeZoneIdentifier(timeZone).
12. Let precision be ToSecondsStringPrecisionRecord(smallestUnit, digits).
13. Let roundedNs be RoundTemporalInstant(instant.[[EpochNanoseconds]], precision.[[Increment]], precision.[[Unit]], roundingMode).
14. Let roundedInstant be ! CreateTemporalInstant(roundedNs).
15. Return TemporalInstantToString(roundedInstant, timeZone, precision.[[Precision]]).

# 8.3.12 Temporal.Instant.prototype.toLocaleString ( [ locales [ , options ] ] )

An ECMAScript implementation that includes the ECMA-402 Internationalization API must implement this method as specified in the ECMA-402 specification. If an ECMAScript implementation does not include the ECMA-402 API the following specification of this method is used.

The meanings of the optional parameters to this method are defined in the ECMA-402 specification; implementations that do not include ECMA-402 support must not use those parameter positions for anything else.

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Return TemporalInstantToString(instant, undefined, auto).

# 8.3.13 Temporal.Instant.prototype.toJSON ( )

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Return TemporalInstantToString(instant, undefined, auto).

# 8.3.14 Temporal.Instant.prototype.valueOf ( )

This method performs the following steps when called:

1. Throw a TypeError exception.

Note

This method always throws, because in the absence of `valueOf()`, expressions with arithmetic operators such as `instant1 > instant2` would fall back to being equivalent to `instant1.toString() > instant2.toString()`. Lexicographical comparison of serialized strings might not seem obviously wrong, because the result would sometimes be correct. Implementations are encouraged to phrase the error message to point users to `Temporal.Instant.compare()`, `Temporal.Instant.prototype.equals()`, and/or `Temporal.Instant.prototype.toString()`.

# 8.3.15 Temporal.Instant.prototype.toZonedDateTimeISO ( timeZone )

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Set timeZone to ? ToTemporalTimeZoneIdentifier(timeZone).
4. Return ! CreateTemporalZonedDateTime(instant.[[EpochNanoseconds]], timeZone, "iso8601").

# 8.4 Properties of Temporal.Instant Instances

Temporal.Instant instances are ordinary objects that inherit properties from the %Temporal.Instant.prototype% intrinsic object. Temporal.Instant instances are initially created with the internal slots described in Table 14.

| Table 14: Internal Slots of Temporal.Instant Instances Internal Slot | Description                                                                                              |
| -------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| [[InitializedTemporalInstant]]                                       | The only specified use of this slot is for distinguishing Temporal.Instant instances from other objects. |
| [[EpochNanoseconds]]                                                 | A BigInt representing the number of nanoseconds since the epoch.                                         |

# 8.4.1 Temporal.Instant range

The [[EpochNanoseconds]] internal slot of a Temporal.Instant object supports a range of exactly -100,000,000 to 100,000,000 days relative to midnight at the beginning of 1 January 1970 UTC, as in 21.4.1.1.

The exact moment of midnight at the beginning of 1 January 1970 UTC is represented by the value 0ℤ.

The maximum value is ℤ(nsMaxInstant), where

nsMaxInstant = 108 × nsPerDay = 8.64 × 1021

where the number of nanoseconds per day is

nsPerDay = 106 × ℝ(msPerDay) = 8.64 × 1013

The minimum value is ℤ(nsMinInstant), where

nsMinInstant = -nsMaxInstant = -8.64 × 1021

# 8.5 Abstract Operations

# 8.5.1 IsValidEpochNanoseconds ( epochNanoseconds )

The abstract operation IsValidEpochNanoseconds takes argument epochNanoseconds (a BigInt) and returns a Boolean. It returns true if its argument is within the allowed range of nanoseconds since the epoch for a Temporal.Instant and Temporal.ZonedDateTime, and false otherwise. It performs the following steps when called:

1. If ℝ(epochNanoseconds) < nsMinInstant or ℝ(epochNanoseconds) > nsMaxInstant, return false; else return true.

# 8.5.2 CreateTemporalInstant ( epochNanoseconds [ , newTarget ] )

The abstract operation CreateTemporalInstant takes argument epochNanoseconds (a BigInt) and optional argument newTarget (a constructor) and returns either a normal completion containing a Temporal.Instant or a throw completion. It creates a Temporal.Instant instance and fills the internal slots with valid values. It performs the following steps when called:

1. Assert: IsValidEpochNanoseconds(epochNanoseconds) is true.
2. If newTarget is not present, set newTarget to %Temporal.Instant%.
3. Let object be ? OrdinaryCreateFromConstructor(newTarget, "%Temporal.Instant.prototype%", « [[InitializedTemporalInstant]], [[EpochNanoseconds]] »).
4. Set object.[[EpochNanoseconds]] to epochNanoseconds.
5. Return object.

# 8.5.3 ToTemporalInstant ( item )

The abstract operation ToTemporalInstant takes argument item (an ECMAScript language value) and returns either a normal completion containing a Temporal.Instant or a throw completion. Converts item to a new Temporal.Instant instance if possible, and throws otherwise. It performs the following steps when called:

1. If item is an Object, then
   1. If item has an [[InitializedTemporalInstant]] or [[InitializedTemporalZonedDateTime]] internal slot, then
      1. Return ! CreateTemporalInstant(item.[[EpochNanoseconds]]).
   2. NOTE: This use of ToPrimitive allows Instant-like objects to be converted.
   3. Set item to ? ToPrimitive(item, string).
2. If item is not a String, throw a TypeError exception.
3. Let parsed be ? ParseISODateTime(item, « TemporalInstantString »).
4. Assert: Either parsed.[[TimeZone]].[[OffsetString]] is not empty or parsed.[[TimeZone]].[[Z]] is true, but not both.
5. If parsed.[[TimeZone]].[[Z]] is true, let offsetNanoseconds be 0; else let offsetNanoseconds be ! ParseDateTimeUTCOffset(parsed.[[TimeZone]].[[OffsetString]]).
6. Let time be parsed.[[Time]].
7. Assert: time is not start-of-day.
8. Let balanced be BalanceISODateTime(parsed.[[Year]], parsed.[[Month]], parsed.[[Day]], time.[[Hour]], time.[[Minute]], time.[[Second]], time.[[Millisecond]], time.[[Microsecond]], time.[[Nanosecond]] \- offsetNanoseconds).
9. Perform ? CheckISODaysRange(balanced.[[ISODate]]).
10. Let epochNanoseconds be GetUTCEpochNanoseconds(balanced).
11. If IsValidEpochNanoseconds(epochNanoseconds) is false, throw a RangeError exception.
12. Return ! CreateTemporalInstant(epochNanoseconds).

# 8.5.4 CompareEpochNanoseconds ( epochNanosecondsOne, epochNanosecondsTwo )

The abstract operation CompareEpochNanoseconds takes arguments epochNanosecondsOne (a BigInt) and epochNanosecondsTwo (a BigInt) and returns either -1, 0, or 1. It compares two epoch nanoseconds values, returning 1 if the first is later than the second, 0 if they are the same, or -1 if the first is earlier. It performs the following steps when called:

1. If epochNanosecondsOne > epochNanosecondsTwo, return 1.
2. If epochNanosecondsOne < epochNanosecondsTwo, return -1.
3. Return 0.

# 8.5.5 AddInstant ( epochNanoseconds, timeDuration )

The abstract operation AddInstant takes arguments epochNanoseconds (a BigInt) and timeDuration (a time duration) and returns either a normal completion containing a BigInt or a throw completion. It adds a duration to a number of nanoseconds since the epoch. It performs the following steps when called:

1. Let result be AddTimeDurationToEpochNanoseconds(timeDuration, epochNanoseconds).
2. If IsValidEpochNanoseconds(result) is false, throw a RangeError exception.
3. Return result.

# 8.5.6 DifferenceInstant ( ns1, ns2, roundingIncrement, smallestUnit, roundingMode )

The abstract operation DifferenceInstant takes arguments ns1 (a BigInt), ns2 (a BigInt), roundingIncrement (a positive integer), smallestUnit (a time unit), and roundingMode (a rounding mode) and returns an Internal Duration Record. It computes the difference between two exact times ns1 and ns2 expressed in nanoseconds since the epoch, and rounds the result according to the parameters roundingIncrement, smallestUnit, and roundingMode. It performs the following steps when called:

1. Let timeDuration be TimeDurationFromEpochNanosecondsDifference(ns2, ns1).
2. Set timeDuration to ! RoundTimeDuration(timeDuration, roundingIncrement, smallestUnit, roundingMode).
3. Return CombineDateAndTimeDuration(ZeroDateDuration(), timeDuration).

# 8.5.7 RoundTemporalInstant ( ns, increment, unit, roundingMode )

The abstract operation RoundTemporalInstant takes arguments ns (a BigInt), increment (a positive integer), unit (a time unit), and roundingMode (a rounding mode) and returns a BigInt. It rounds a number of nanoseconds ns since the epoch to the given rounding increment. It performs the following steps when called:

1. Let unitLength be the value in the "Length in Nanoseconds" column of the row of Table 21 whose "Value" column contains unit.
2. Let incrementNs be increment × unitLength.
3. Return ℤ(RoundNumberToIncrementAsIfPositive(ℝ(ns), incrementNs, roundingMode)).

# 8.5.8 TemporalInstantToString ( instant, timeZone, precision )

The abstract operation TemporalInstantToString takes arguments instant (a Temporal.Instant), timeZone (either an available time zone identifier or undefined), and precision (either an integer in the inclusive interval from 0 to 9, minute, or auto) and returns a String. It formats instant as an ISO 8601 / RFC 9557 string, to the precision specified by precision, using the UTC offset of timeZone, or `Z` if timeZone is undefined. It performs the following steps when called:

1. Let outputTimeZone be timeZone.
2. If outputTimeZone is undefined, set outputTimeZone to "UTC".
3. Let epochNs be instant.[[EpochNanoseconds]].
4. Let isoDateTime be GetISODateTimeFor(outputTimeZone, epochNs).
5. Let dateTimeString be ISODateTimeToString(isoDateTime, "iso8601", precision, never).
6. If timeZone is undefined, then
   1. Let timeZoneString be "Z".
7. Else,
   1. Let offsetNanoseconds be GetOffsetNanosecondsFor(outputTimeZone, epochNs).
   2. Let timeZoneString be FormatDateTimeUTCOffsetRounded(offsetNanoseconds).
8. Return the string-concatenation of dateTimeString and timeZoneString.

# 8.5.9 DifferenceTemporalInstant ( operation, instant, other, options )

The abstract operation DifferenceTemporalInstant takes arguments operation (either since or until), instant (a Temporal.Instant), other (an ECMAScript language value), and options (an ECMAScript language value) and returns either a normal completion containing a Temporal.Duration or a throw completion. It computes the difference between the two times represented by instant and other, optionally rounds it, and returns it as a Temporal.Duration object. It performs the following steps when called:

1. Set other to ? ToTemporalInstant(other).
2. Let resolvedOptions be ? GetOptionsObject(options).
3. Let settings be ? GetDifferenceSettings(operation, resolvedOptions, time, « », nanosecond, second).
4. Let internalDuration be DifferenceInstant(instant.[[EpochNanoseconds]], other.[[EpochNanoseconds]], settings.[[RoundingIncrement]], settings.[[SmallestUnit]], settings.[[RoundingMode]]).
5. Let result be ! TemporalDurationFromInternal(internalDuration, settings.[[LargestUnit]]).
6. If operation is since, set result to CreateNegatedTemporalDuration(result).
7. Return result.

# 8.5.10 AddDurationToInstant ( operation, instant, temporalDurationLike )

The abstract operation AddDurationToInstant takes arguments operation (either add or subtract), instant (a Temporal.Instant), and temporalDurationLike (an ECMAScript language value) and returns either a normal completion containing a Temporal.Instant or a throw completion. It adds/subtracts temporalDurationLike to/from instant. It performs the following steps when called:

1. Let duration be ? ToTemporalDuration(temporalDurationLike).
2. If operation is subtract, set duration to CreateNegatedTemporalDuration(duration).
3. Let largestUnit be DefaultTemporalLargestUnit(duration).
4. If TemporalUnitCategory(largestUnit) is date, throw a RangeError exception.
5. Let internalDuration be ToInternalDurationRecordWith24HourDays(duration).
6. Let ns be ? AddInstant(instant.[[EpochNanoseconds]], internalDuration.[[Time]]).
7. Return ! CreateTemporalInstant(ns).

# 9 Temporal.PlainYearMonth Objects

A Temporal.PlainYearMonth object is an Object that contains integers corresponding to a particular year and month in a particular calendar.

# 9.1 The Temporal.PlainYearMonth Constructor

The Temporal.PlainYearMonth constructor:

- creates and initializes a new Temporal.PlainYearMonth object when called as a constructor.
- is not intended to be called as a function and will throw an exception when called in that manner.
- may be used as the value of an `extends` clause of a class definition. Subclass constructors that intend to inherit the specified Temporal.PlainYearMonth behaviour must include a super call to the %Temporal.PlainYearMonth% constructor to create and initialize subclass instances with the necessary internal slots.

# 9.1.1 Temporal.PlainYearMonth ( isoYear, isoMonth [ , calendar [ , referenceISODay ] ] )

This function performs the following steps when called:

1. If NewTarget is undefined, throw a TypeError exception.
2. If referenceISODay is undefined, then
   1. Set referenceISODay to 1𝔽.
3. Let y be ? ToIntegerWithTruncation(isoYear).
4. Let m be ? ToIntegerWithTruncation(isoMonth).
5. If calendar is undefined, set calendar to "iso8601".
6. If calendar is not a String, throw a TypeError exception.
7. Set calendar to ? CanonicalizeCalendar(calendar).
8. Let ref be ? ToIntegerWithTruncation(referenceISODay).
9. If IsValidISODate(y, m, ref) is false, throw a RangeError exception.
10. Let isoDate be CreateISODateRecord(y, m, ref).
11. Return ? CreateTemporalYearMonth(isoDate, calendar, NewTarget).

# 9.2 Properties of the Temporal.PlainYearMonth Constructor

The value of the [[Prototype]] internal slot of the Temporal.PlainYearMonth constructor is the intrinsic object %Function.prototype%.

The Temporal.PlainYearMonth constructor has the following properties:

# 9.2.1 Temporal.PlainYearMonth.prototype

The initial value of `Temporal.PlainYearMonth.prototype` is %Temporal.PlainYearMonth.prototype%.

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: false }.

# 9.2.2 Temporal.PlainYearMonth.from ( item [ , options ] )

This function performs the following steps when called:

1. Return ? ToTemporalYearMonth(item, options).

# 9.2.3 Temporal.PlainYearMonth.compare ( one, two )

This function performs the following steps when called:

1. Set one to ? ToTemporalYearMonth(one).
2. Set two to ? ToTemporalYearMonth(two).
3. Return 𝔽(CompareISODate(one.[[ISODate]], two.[[ISODate]])).

# 9.3 Properties of the Temporal.PlainYearMonth Prototype Object

The Temporal.PlainYearMonth prototype object

- is itself an ordinary object.
- is not a Temporal.PlainYearMonth instance and does not have a [[InitializedTemporalYearMonth]] internal slot.
- has a [[Prototype]] internal slot whose value is %Object.prototype%.

Note

An ECMAScript implementation that includes the ECMA-402 Internationalization API extends this prototype with additional properties in order to represent calendar data.

# 9.3.1 Temporal.PlainYearMonth.prototype.constructor

The initial value of `Temporal.PlainYearMonth.prototype.constructor` is %Temporal.PlainYearMonth%.

# 9.3.2 Temporal.PlainYearMonth.prototype[ %Symbol.toStringTag% ]

The initial value of the %Symbol.toStringTag% property is the String "Temporal.PlainYearMonth".

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }.

# 9.3.3 get Temporal.PlainYearMonth.prototype.calendarId

`Temporal.PlainYearMonth.prototype.calendarId` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return plainYearMonth.[[Calendar]].

# 9.3.4 get Temporal.PlainYearMonth.prototype.era

`Temporal.PlainYearMonth.prototype.era` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return CalendarISOToDate(plainYearMonth.[[Calendar]], plainYearMonth.[[ISODate]]).[[Era]].

# 9.3.5 get Temporal.PlainYearMonth.prototype.eraYear

`Temporal.PlainYearMonth.prototype.eraYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Let result be CalendarISOToDate(plainYearMonth.[[Calendar]], plainYearMonth.[[ISODate]]).[[EraYear]].
4. If result is undefined, return undefined.
5. Return 𝔽(result).

# 9.3.6 get Temporal.PlainYearMonth.prototype.year

`Temporal.PlainYearMonth.prototype.year` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return 𝔽(CalendarISOToDate(plainYearMonth.[[Calendar]], plainYearMonth.[[ISODate]]).[[Year]]).

# 9.3.7 get Temporal.PlainYearMonth.prototype.month

`Temporal.PlainYearMonth.prototype.month` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return 𝔽(CalendarISOToDate(plainYearMonth.[[Calendar]], plainYearMonth.[[ISODate]]).[[Month]]).

# 9.3.8 get Temporal.PlainYearMonth.prototype.monthCode

`Temporal.PlainYearMonth.prototype.monthCode` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return CalendarISOToDate(plainYearMonth.[[Calendar]], plainYearMonth.[[ISODate]]).[[MonthCode]].

# 9.3.9 get Temporal.PlainYearMonth.prototype.daysInYear

`Temporal.PlainYearMonth.prototype.daysInYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return 𝔽(CalendarISOToDate(plainYearMonth.[[Calendar]], plainYearMonth.[[ISODate]]).[[DaysInYear]]).

# 9.3.10 get Temporal.PlainYearMonth.prototype.daysInMonth

`Temporal.PlainYearMonth.prototype.daysInMonth` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return 𝔽(CalendarISOToDate(plainYearMonth.[[Calendar]], plainYearMonth.[[ISODate]]).[[DaysInMonth]]).

# 9.3.11 get Temporal.PlainYearMonth.prototype.monthsInYear

`Temporal.PlainYearMonth.prototype.monthsInYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return 𝔽(CalendarISOToDate(plainYearMonth.[[Calendar]], plainYearMonth.[[ISODate]]).[[MonthsInYear]]).

# 9.3.12 get Temporal.PlainYearMonth.prototype.inLeapYear

`Temporal.PlainYearMonth.prototype.inLeapYear` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return CalendarISOToDate(plainYearMonth.[[Calendar]], plainYearMonth.[[ISODate]]).[[InLeapYear]].

# 9.3.13 Temporal.PlainYearMonth.prototype.with ( temporalYearMonthLike [ , options ] )

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. If ? IsPartialTemporalObject(temporalYearMonthLike) is false, throw a TypeError exception.
4. Let calendar be plainYearMonth.[[Calendar]].
5. Let fields be ISODateToFields(calendar, plainYearMonth.[[ISODate]], year-month).
6. Let partialYearMonth be ? PrepareCalendarFields(calendar, temporalYearMonthLike, « year, month, month-code », « », partial).
7. Set fields to CalendarMergeFields(calendar, fields, partialYearMonth).
8. Let resolvedOptions be ? GetOptionsObject(options).
9. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
10. Let isoDate be ? CalendarYearMonthFromFields(calendar, fields, overflow).
11. Return ! CreateTemporalYearMonth(isoDate, calendar).

# 9.3.14 Temporal.PlainYearMonth.prototype.add ( temporalDurationLike [ , options ] )

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return ? AddDurationToYearMonth(add, plainYearMonth, temporalDurationLike, options).

# 9.3.15 Temporal.PlainYearMonth.prototype.subtract ( temporalDurationLike [ , options ] )

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return ? AddDurationToYearMonth(subtract, plainYearMonth, temporalDurationLike, options).

# 9.3.16 Temporal.PlainYearMonth.prototype.until ( other [ , options ] )

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return ? DifferenceTemporalPlainYearMonth(until, plainYearMonth, other, options).

# 9.3.17 Temporal.PlainYearMonth.prototype.since ( other [ , options ] )

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return ? DifferenceTemporalPlainYearMonth(since, plainYearMonth, other, options).

# 9.3.18 Temporal.PlainYearMonth.prototype.equals ( other )

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Set other to ? ToTemporalYearMonth(other).
4. If CompareISODate(plainYearMonth.[[ISODate]], other.[[ISODate]]) ≠ 0, return false.
5. Return CalendarEquals(plainYearMonth.[[Calendar]], other.[[Calendar]]).

# 9.3.19 Temporal.PlainYearMonth.prototype.toString ( [ options ] )

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. Let showCalendar be ? GetTemporalShowCalendarNameOption(resolvedOptions).
5. Return TemporalYearMonthToString(plainYearMonth, showCalendar).

# 9.3.20 Temporal.PlainYearMonth.prototype.toLocaleString ( [ locales [ , options ] ] )

An ECMAScript implementation that includes the ECMA-402 Internationalization API must implement this method as specified in the ECMA-402 specification. If an ECMAScript implementation does not include the ECMA-402 API the following specification of this method is used.

The meanings of the optional parameters to this method are defined in the ECMA-402 specification; implementations that do not include ECMA-402 support must not use those parameter positions for anything else.

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return TemporalYearMonthToString(plainYearMonth, auto).

# 9.3.21 Temporal.PlainYearMonth.prototype.toJSON ( )

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Return TemporalYearMonthToString(plainYearMonth, auto).

# 9.3.22 Temporal.PlainYearMonth.prototype.valueOf ( )

This method performs the following steps when called:

1. Throw a TypeError exception.

Note

This method always throws, because in the absence of `valueOf()`, expressions with arithmetic operators such as `plainYearMonth1 > plainYearMonth2` would fall back to being equivalent to `plainYearMonth1.toString() > plainYearMonth2.toString()`. Lexicographical comparison of serialized strings might not seem obviously wrong, because the result would sometimes be correct. Implementations are encouraged to phrase the error message to point users to `Temporal.PlainYearMonth.compare()`, `Temporal.PlainYearMonth.prototype.equals()`, and/or `Temporal.PlainYearMonth.prototype.toString()`.

# 9.3.23 Temporal.PlainYearMonth.prototype.toPlainDate ( item )

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. If item is not an Object, throw a TypeError exception.
4. Let calendar be plainYearMonth.[[Calendar]].
5. Let fields be ISODateToFields(calendar, plainYearMonth.[[ISODate]], year-month).
6. Let inputFields be ? PrepareCalendarFields(calendar, item, « day », « », « »).
7. Let mergedFields be CalendarMergeFields(calendar, fields, inputFields).
8. Let isoDate be ? CalendarDateFromFields(calendar, mergedFields, constrain).
9. Return ! CreateTemporalDate(isoDate, calendar).

# 9.4 Properties of Temporal.PlainYearMonth Instances

Temporal.PlainYearMonth instances are ordinary objects that inherit properties from the %Temporal.PlainYearMonth.prototype% intrinsic object. Temporal.PlainYearMonth instances are initially created with the internal slots described in Table 15.

| Table 15: Internal Slots of Temporal.PlainYearMonth Instances Internal Slot | Description                                                                                                                                                                                                            |
| --------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [[InitializedTemporalYearMonth]]                                            | The only specified use of this slot is for distinguishing Temporal.PlainYearMonth instances from other objects.                                                                                                        |
| [[ISODate]]                                                                 | An ISO Date Record. The [[Day]] field is used by the calendar in the [[Calendar]] slot to disambiguate if the [[Year]] and [[Month]] fields are not sufficient to uniquely identify a year and month in that calendar. |
| [[Calendar]]                                                                | A calendar type.                                                                                                                                                                                                       |

# 9.5 Abstract Operations

# 9.5.1 ISO Year-Month Records

An ISO Year-Month Record is a Record value used to represent a valid month in the ISO 8601 calendar, although the year may be outside of the allowed range for Temporal.

ISO Year-Month Records have the fields listed in Table 16.

| Table 16: ISO Year-Month Record Fields Field Name | Value                                  | Meaning                                           |
| ------------------------------------------------- | -------------------------------------- | ------------------------------------------------- |
| [[Year]]                                          | an integer                             | The year in the ISO 8601 calendar.                |
| [[Month]]                                         | an integer between 1 and 12, inclusive | The number of the month in the ISO 8601 calendar. |

# 9.5.2 ToTemporalYearMonth ( item [ , options ] )

The abstract operation ToTemporalYearMonth takes argument item (an ECMAScript language value) and optional argument options (an ECMAScript language value) and returns either a normal completion containing a Temporal.PlainYearMonth, or a throw completion. Converts item to a new Temporal.PlainYearMonth instance if possible, and throws otherwise. It performs the following steps when called:

1. If options is not present, set options to undefined.
2. If item is an Object, then
   1. If item has an [[InitializedTemporalYearMonth]] internal slot, then
      1. Let resolvedOptions be ? GetOptionsObject(options).
      2. Perform ? GetTemporalOverflowOption(resolvedOptions).
      3. Return ! CreateTemporalYearMonth(item.[[ISODate]], item.[[Calendar]]).
   2. Let calendar be ? GetTemporalCalendarIdentifierWithISODefault(item).
   3. Let fields be ? PrepareCalendarFields(calendar, item, « year, month, month-code », «», «»).
   4. Let resolvedOptions be ? GetOptionsObject(options).
   5. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
   6. Let isoDate be ? CalendarYearMonthFromFields(calendar, fields, overflow).
   7. Return ! CreateTemporalYearMonth(isoDate, calendar).
3. If item is not a String, throw a TypeError exception.
4. Let result be ? ParseISODateTime(item, « TemporalYearMonthString »).
5. Let calendar be result.[[Calendar]].
6. If calendar is empty, set calendar to "iso8601".
7. Set calendar to ? CanonicalizeCalendar(calendar).
8. Let resolvedOptions be ? GetOptionsObject(options).
9. Perform ? GetTemporalOverflowOption(resolvedOptions).
10. Let isoDate be CreateISODateRecord(result.[[Year]], result.[[Month]], result.[[Day]]).
11. If ISOYearMonthWithinLimits(isoDate) is false, throw a RangeError exception.
12. Set result to ISODateToFields(calendar, isoDate, year-month).
13. NOTE: The following operation is called with constrain regardless of overflow, in order for the calendar to store a canonical value in the [[Day]] field of the [[ISODate]] internal slot of the result.
14. Set isoDate to ? CalendarYearMonthFromFields(calendar, result, constrain).
15. Return ! CreateTemporalYearMonth(isoDate, calendar).

# 9.5.3 ISOYearMonthWithinLimits ( isoDate )

The abstract operation ISOYearMonthWithinLimits takes argument isoDate (an ISO Date Record) and returns a Boolean. It returns true if its argument represents a month within the range that a Temporal.PlainYearMonth object can represent, and false otherwise. It performs the following steps when called:

1. If isoDate.[[Year]] < -271821 or isoDate.[[Year]] > 275760, return false.
2. If isoDate.[[Year]] = -271821 and isoDate.[[Month]] < 4, return false.
3. If isoDate.[[Year]] = 275760 and isoDate.[[Month]] > 9, return false.
4. Return true.

Note

Temporal.PlainYearMonth objects can represent any month that contains a day that a Temporal.PlainDate can represent. This ensures that a Temporal.PlainDate object can always be converted into a Temporal.PlainYearMonth object.

# 9.5.4 BalanceISOYearMonth ( year, month )

The abstract operation BalanceISOYearMonth takes arguments year (an integer) and month (an integer) and returns an ISO Year-Month Record. It balances year and month so that month falls within the range 1 to 12, carrying over excess into year. It performs the following steps when called:

1. Set year to year \+ floor((month \- 1) / 12).
2. Set month to ((month \- 1) modulo 12) + 1.
3. Return ISO Year-Month Record { [[Year]]: year, [[Month]]: month }.

# 9.5.5 CreateTemporalYearMonth ( isoDate, calendar [ , newTarget ] )

The abstract operation CreateTemporalYearMonth takes arguments isoDate (an ISO Date Record) and calendar (a calendar type) and optional argument newTarget (a constructor) and returns either a normal completion containing a Temporal.PlainYearMonth or a throw completion. It creates a Temporal.PlainYearMonth instance and fills the internal slots with valid values. It performs the following steps when called:

1. If ISOYearMonthWithinLimits(isoDate) is false, throw a RangeError exception.
2. If newTarget is not present, set newTarget to %Temporal.PlainYearMonth%.
3. Let object be ? OrdinaryCreateFromConstructor(newTarget, "%Temporal.PlainYearMonth.prototype%", « [[InitializedTemporalYearMonth]], [[ISODate]], [[Calendar]] »).
4. Set object.[[ISODate]] to isoDate.
5. Set object.[[Calendar]] to calendar.
6. Return object.

# 9.5.6 TemporalYearMonthToString ( yearMonth, showCalendar )

The abstract operation TemporalYearMonthToString takes arguments yearMonth (a Temporal.PlainYearMonth) and showCalendar (one of auto, always, never, or critical) and returns a String. It formats yearMonth as an ISO 8601 / RFC 9557 string. It performs the following steps when called:

1. Let year be PadISOYear(yearMonth.[[ISODate]].[[Year]]).
2. Let month be ToZeroPaddedDecimalString(yearMonth.[[ISODate]].[[Month]], 2).
3. Let result be the string-concatenation of year, the code unit 0x002D (HYPHEN-MINUS), and month.
4. If showCalendar is one of always or critical, or yearMonth.[[Calendar]] is not "iso8601", then
   1. Let day be ToZeroPaddedDecimalString(yearMonth.[[ISODate]].[[Day]], 2).
   2. Set result to the string-concatenation of result, the code unit 0x002D (HYPHEN-MINUS), and day.
5. Let calendarString be FormatCalendarAnnotation(yearMonth.[[Calendar]], showCalendar).
6. Set result to the string-concatenation of result and calendarString.
7. Return result.

# 9.5.7 DifferenceTemporalPlainYearMonth ( operation, yearMonth, other, options )

The abstract operation DifferenceTemporalPlainYearMonth takes arguments operation (either since or until), yearMonth (a Temporal.PlainYearMonth), other (an ECMAScript language value), and options (an ECMAScript language value) and returns either a normal completion containing a Temporal.Duration or a throw completion. It computes the difference between the two times represented by yearMonth and other, optionally rounds it, and returns it as a Temporal.Duration object. It performs the following steps when called:

1. Set other to ? ToTemporalYearMonth(other).
2. Let calendar be yearMonth.[[Calendar]].
3. If CalendarEquals(calendar, other.[[Calendar]]) is false, throw a RangeError exception.
4. Let resolvedOptions be ? GetOptionsObject(options).
5. Let settings be ? GetDifferenceSettings(operation, resolvedOptions, date, « week, day », month, year).
6. If CompareISODate(yearMonth.[[ISODate]], other.[[ISODate]]) = 0, return ! CreateTemporalDuration(0, 0, 0, 0, 0, 0, 0, 0, 0, 0).
7. Let thisFields be ISODateToFields(calendar, yearMonth.[[ISODate]], year-month).
8. Set thisFields.[[Day]] to 1.
9. Let thisDate be ? CalendarDateFromFields(calendar, thisFields, constrain).
10. Let otherFields be ISODateToFields(calendar, other.[[ISODate]], year-month).
11. Set otherFields.[[Day]] to 1.
12. Let otherDate be ? CalendarDateFromFields(calendar, otherFields, constrain).
13. Let dateDifference be CalendarDateUntil(calendar, thisDate, otherDate, settings.[[LargestUnit]]).
14. Let yearsMonthsDifference be ! AdjustDateDurationRecord(dateDifference, 0, 0).
15. Let duration be CombineDateAndTimeDuration(yearsMonthsDifference, 0).
16. If settings.[[SmallestUnit]] is not month or settings.[[RoundingIncrement]] ≠ 1, then
    1. Let isoDateTime be CombineISODateAndTimeRecord(thisDate, MidnightTimeRecord()).
    2. Let originEpochNs be GetUTCEpochNanoseconds(isoDateTime).
    3. Let isoDateTimeOther be CombineISODateAndTimeRecord(otherDate, MidnightTimeRecord()).
    4. Let destEpochNs be GetUTCEpochNanoseconds(isoDateTimeOther).
    5. Set duration to ? RoundRelativeDuration(duration, originEpochNs, destEpochNs, isoDateTime, unset, calendar, settings.[[LargestUnit]], settings.[[RoundingIncrement]], settings.[[SmallestUnit]], settings.[[RoundingMode]]).
17. Let result be ! TemporalDurationFromInternal(duration, day).
18. If operation is since, set result to CreateNegatedTemporalDuration(result).
19. Return result.

# 9.5.8 AddDurationToYearMonth ( operation, yearMonth, temporalDurationLike, options )

The abstract operation AddDurationToYearMonth takes arguments operation (either add or subtract), yearMonth (a Temporal.PlainYearMonth), temporalDurationLike (an ECMAScript language value), and options (an ECMAScript language value) and returns either a normal completion containing a Temporal.PlainYearMonth or a throw completion. It adds/subtracts temporalDurationLike to/from yearMonth, returning a point in time that is in the future/past relative to yearMonth. It performs the following steps when called:

1. Let duration be ? ToTemporalDuration(temporalDurationLike).
2. If operation is subtract, set duration to CreateNegatedTemporalDuration(duration).
3. Let internalDuration be ToInternalDurationRecord(duration).
4. Let resolvedOptions be ? GetOptionsObject(options).
5. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
6. Let durationToAdd be internalDuration.[[Date]].
7. If durationToAdd.[[Weeks]] ≠ 0, or durationToAdd.[[Days]] ≠ 0, or internalDuration.[[Time]] ≠ 0, throw a RangeError exception.
8. Let calendar be yearMonth.[[Calendar]].
9. Let fields be ISODateToFields(calendar, yearMonth.[[ISODate]], year-month).
10. Set fields.[[Day]] to 1.
11. Let date be ? CalendarDateFromFields(calendar, fields, constrain).
12. Let addedDate be ? CalendarDateAdd(calendar, date, durationToAdd, overflow).
13. Let addedDateFields be ISODateToFields(calendar, addedDate, year-month).
14. Let isoDate be ? CalendarYearMonthFromFields(calendar, addedDateFields, overflow).
15. Return ! CreateTemporalYearMonth(isoDate, calendar).

# 10 Temporal.PlainMonthDay Objects

A Temporal.PlainMonthDay object is an Object that contains integers corresponding to a particular month and day in a particular calendar.

# 10.1 The Temporal.PlainMonthDay Constructor

The Temporal.PlainMonthDay constructor:

- creates and initializes a new Temporal.PlainMonthDay object when called as a constructor.
- is not intended to be called as a function and will throw an exception when called in that manner.
- may be used as the value of an `extends` clause of a class definition. Subclass constructors that intend to inherit the specified Temporal.PlainMonthDay behaviour must include a super call to the %Temporal.PlainMonthDay% constructor to create and initialize subclass instances with the necessary internal slots.

# 10.1.1 Temporal.PlainMonthDay ( isoMonth, isoDay [ , calendar [ , referenceISOYear ] ] )

This function performs the following steps when called:

1. If NewTarget is undefined, throw a TypeError exception.
2. If referenceISOYear is undefined, then
   1. Set referenceISOYear to 1972𝔽 (the first ISO 8601 leap year after the epoch).
3. Let m be ? ToIntegerWithTruncation(isoMonth).
4. Let d be ? ToIntegerWithTruncation(isoDay).
5. If calendar is undefined, set calendar to "iso8601".
6. If calendar is not a String, throw a TypeError exception.
7. Set calendar to ? CanonicalizeCalendar(calendar).
8. Let y be ? ToIntegerWithTruncation(referenceISOYear).
9. If IsValidISODate(y, m, d) is false, throw a RangeError exception.
10. Let isoDate be CreateISODateRecord(y, m, d).
11. Return ? CreateTemporalMonthDay(isoDate, calendar, NewTarget).

# 10.2 Properties of the Temporal.PlainMonthDay Constructor

The value of the [[Prototype]] internal slot of the Temporal.PlainMonthDay constructor is the intrinsic object %Function.prototype%.

The Temporal.PlainMonthDay constructor has the following properties:

# 10.2.1 Temporal.PlainMonthDay.prototype

The initial value of `Temporal.PlainMonthDay.prototype` is %Temporal.PlainMonthDay.prototype%.

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: false }.

# 10.2.2 Temporal.PlainMonthDay.from ( item [ , options ] )

This function performs the following steps when called:

1. Return ? ToTemporalMonthDay(item, options).

# 10.3 Properties of the Temporal.PlainMonthDay Prototype Object

The Temporal.PlainMonthDay prototype object

- is itself an ordinary object.
- is not a Temporal.PlainMonthDay instance and does not have a [[InitializedTemporalMonthDay]] internal slot.
- has a [[Prototype]] internal slot whose value is %Object.prototype%.

Note

An ECMAScript implementation that includes the ECMA-402 Internationalization API extends this prototype with additional properties in order to represent calendar data.

# 10.3.1 Temporal.PlainMonthDay.prototype.constructor

The initial value of `Temporal.PlainMonthDay.prototype.constructor` is %Temporal.PlainMonthDay%.

# 10.3.2 Temporal.PlainMonthDay.prototype[ %Symbol.toStringTag% ]

The initial value of the %Symbol.toStringTag% property is the String "Temporal.PlainMonthDay".

This property has the attributes { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }.

# 10.3.3 get Temporal.PlainMonthDay.prototype.calendarId

`Temporal.PlainMonthDay.prototype.calendarId` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainMonthDay be the this value.
2. Perform ? RequireInternalSlot(plainMonthDay, [[InitializedTemporalMonthDay]]).
3. Return plainMonthDay.[[Calendar]].

# 10.3.4 get Temporal.PlainMonthDay.prototype.monthCode

`Temporal.PlainMonthDay.prototype.monthCode` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainMonthDay be the this value.
2. Perform ? RequireInternalSlot(plainMonthDay, [[InitializedTemporalMonthDay]]).
3. Return CalendarISOToDate(plainMonthDay.[[Calendar]], plainMonthDay.[[ISODate]]).[[MonthCode]].

# 10.3.5 get Temporal.PlainMonthDay.prototype.day

`Temporal.PlainMonthDay.prototype.day` is an accessor property whose set accessor function is undefined. Its get accessor function performs the following steps:

1. Let plainMonthDay be the this value.
2. Perform ? RequireInternalSlot(plainMonthDay, [[InitializedTemporalMonthDay]]).
3. Return 𝔽(CalendarISOToDate(plainMonthDay.[[Calendar]], plainMonthDay.[[ISODate]]).[[Day]]).

# 10.3.6 Temporal.PlainMonthDay.prototype.with ( temporalMonthDayLike [ , options ] )

This method performs the following steps when called:

1. Let plainMonthDay be the this value.
2. Perform ? RequireInternalSlot(plainMonthDay, [[InitializedTemporalMonthDay]]).
3. If ? IsPartialTemporalObject(temporalMonthDayLike) is false, throw a TypeError exception.
4. Let calendar be plainMonthDay.[[Calendar]].
5. Let fields be ISODateToFields(calendar, plainMonthDay.[[ISODate]], month-day).
6. Let partialMonthDay be ? PrepareCalendarFields(calendar, temporalMonthDayLike, « year, month, month-code, day », « », partial).
7. Set fields to CalendarMergeFields(calendar, fields, partialMonthDay).
8. Let resolvedOptions be ? GetOptionsObject(options).
9. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
10. Let isoDate be ? CalendarMonthDayFromFields(calendar, fields, overflow).
11. Return ! CreateTemporalMonthDay(isoDate, calendar).

# 10.3.7 Temporal.PlainMonthDay.prototype.equals ( other )

This method performs the following steps when called:

1. Let plainMonthDay be the this value.
2. Perform ? RequireInternalSlot(plainMonthDay, [[InitializedTemporalMonthDay]]).
3. Set other to ? ToTemporalMonthDay(other).
4. If CompareISODate(plainMonthDay.[[ISODate]], other.[[ISODate]]) ≠ 0, return false.
5. Return CalendarEquals(plainMonthDay.[[Calendar]], other.[[Calendar]]).

# 10.3.8 Temporal.PlainMonthDay.prototype.toString ( [ options ] )

This method performs the following steps when called:

1. Let plainMonthDay be the this value.
2. Perform ? RequireInternalSlot(plainMonthDay, [[InitializedTemporalMonthDay]]).
3. Let resolvedOptions be ? GetOptionsObject(options).
4. Let showCalendar be ? GetTemporalShowCalendarNameOption(resolvedOptions).
5. Return TemporalMonthDayToString(plainMonthDay, showCalendar).

# 10.3.9 Temporal.PlainMonthDay.prototype.toLocaleString ( [ locales [ , options ] ] )

An ECMAScript implementation that includes the ECMA-402 Internationalization API must implement this method as specified in the ECMA-402 specification. If an ECMAScript implementation does not include the ECMA-402 API the following specification of this method is used.

The meanings of the optional parameters to this method are defined in the ECMA-402 specification; implementations that do not include ECMA-402 support must not use those parameter positions for anything else.

This method performs the following steps when called:

1. Let plainMonthDay be the this value.
2. Perform ? RequireInternalSlot(plainMonthDay, [[InitializedTemporalMonthDay]]).
3. Return TemporalMonthDayToString(plainMonthDay, auto).

# 10.3.10 Temporal.PlainMonthDay.prototype.toJSON ( )

This method performs the following steps when called:

1. Let plainMonthDay be the this value.
2. Perform ? RequireInternalSlot(plainMonthDay, [[InitializedTemporalMonthDay]]).
3. Return TemporalMonthDayToString(plainMonthDay, auto).

# 10.3.11 Temporal.PlainMonthDay.prototype.valueOf ( )

This method performs the following steps when called:

1. Throw a TypeError exception.

Note

This method always throws, because in the absence of `valueOf()`, expressions with arithmetic operators such as `plainMonthDay1 > plainMonthDay2` would fall back to being equivalent to `plainMonthDay1.toString() > plainMonthDay2.toString()`. Lexicographical comparison of serialized strings might not seem obviously wrong, because the result would sometimes be correct. Implementations are encouraged to phrase the error message to point users to `Temporal.PlainDate.compare()` on the corresponding `PlainDate` objects, `Temporal.PlainMonthDay.prototype.equals()`, and/or `Temporal.PlainMonthDay.prototype.toString()`.

# 10.3.12 Temporal.PlainMonthDay.prototype.toPlainDate ( item )

This method performs the following steps when called:

1. Let plainMonthDay be the this value.
2. Perform ? RequireInternalSlot(plainMonthDay, [[InitializedTemporalMonthDay]]).
3. If item is not an Object, throw a TypeError exception.
4. Let calendar be plainMonthDay.[[Calendar]].
5. Let fields be ISODateToFields(calendar, plainMonthDay.[[ISODate]], month-day).
6. Let inputFields be ? PrepareCalendarFields(calendar, item, « year », « », « »).
7. Let mergedFields be CalendarMergeFields(calendar, fields, inputFields).
8. Let isoDate be ? CalendarDateFromFields(calendar, mergedFields, constrain).
9. Return ! CreateTemporalDate(isoDate, calendar).

# 10.4 Properties of Temporal.PlainMonthDay Instances

Temporal.PlainMonthDay instances are ordinary objects that inherit properties from the %Temporal.PlainMonthDay.prototype% intrinsic object. Temporal.PlainMonthDay instances are initially created with the internal slots described in Table 17.

| Table 17: Internal Slots of Temporal.PlainMonthDay Instances Internal Slot | Description                                                                                                                                                                                                           |
| -------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [[InitializedTemporalMonthDay]]                                            | The only specified use of this slot is for distinguishing Temporal.PlainMonthDay instances from other objects.                                                                                                        |
| [[ISODate]]                                                                | An ISO Date Record. The [[Year]] field is used by the calendar in the [[Calendar]] slot to disambiguate if the [[Month]] and [[Day]] fields are not sufficient to uniquely identify a month and day in that calendar. |
| [[Calendar]]                                                               | A calendar type.                                                                                                                                                                                                      |

# 10.5 Abstract Operations

# 10.5.1 ToTemporalMonthDay ( item [ , options ] )

The abstract operation ToTemporalMonthDay takes argument item (an ECMAScript language value) and optional argument options (an ECMAScript language value) and returns either a normal completion containing a Temporal.PlainMonthDay or a throw completion. Converts item to a new Temporal.PlainMonthDay instance if possible, and throws otherwise. It performs the following steps when called:

1. If options is not present, set options to undefined.
2. If item is an Object, then
   1. If item has an [[InitializedTemporalMonthDay]] internal slot, then
      1. Let resolvedOptions be ? GetOptionsObject(options).
      2. Perform ? GetTemporalOverflowOption(resolvedOptions).
      3. Return ! CreateTemporalMonthDay(item.[[ISODate]], item.[[Calendar]]).
   2. Let calendar be ? GetTemporalCalendarIdentifierWithISODefault(item).
   3. Let fields be ? PrepareCalendarFields(calendar, item, « year, month, month-code, day », «», «»).
   4. Let resolvedOptions be ? GetOptionsObject(options).
   5. Let overflow be ? GetTemporalOverflowOption(resolvedOptions).
   6. Let isoDate be ? CalendarMonthDayFromFields(calendar, fields, overflow).
   7. Return ! CreateTemporalMonthDay(isoDate, calendar).
3. If item is not a String, throw a TypeError exception.
4. Let result be ? ParseISODateTime(item, « TemporalMonthDayString »).
5. Let calendar be result.[[Calendar]].
6. If calendar is empty, set calendar to "iso8601".
7. Set calendar to ? CanonicalizeCalendar(calendar).
8. Let resolvedOptions be ? GetOptionsObject(options).
9. Perform ? GetTemporalOverflowOption(resolvedOptions).
10. If calendar is "iso8601", then
    1. Let referenceISOYear be 1972 (the first ISO 8601 leap year after the epoch).
    2. Let isoDate be CreateISODateRecord(referenceISOYear, result.[[Month]], result.[[Day]]).
    3. Return ! CreateTemporalMonthDay(isoDate, calendar).
11. Let isoDate be CreateISODateRecord(result.[[Year]], result.[[Month]], result.[[Day]]).
12. If ISODateWithinLimits(isoDate) is false, throw a RangeError exception.
13. Set result to ISODateToFields(calendar, isoDate, month-day).
14. NOTE: The following operation is called with constrain regardless of overflow, in order for the calendar to store a canonical value in the [[Year]] field of the [[ISODate]] internal slot of the result.
15. Set isoDate to ? CalendarMonthDayFromFields(calendar, result, constrain).
16. Return ! CreateTemporalMonthDay(isoDate, calendar).

# 10.5.2 CreateTemporalMonthDay ( isoDate, calendar [ , newTarget ] )

The abstract operation CreateTemporalMonthDay takes arguments isoDate (an ISO Date Record) and calendar (a calendar type) and optional argument newTarget (a constructor) and returns either a normal completion containing a Temporal.PlainMonthDay or a throw completion. It creates a Temporal.PlainMonthDay instance and fills the internal slots with valid values. It performs the following steps when called:

1. If ISODateWithinLimits(isoDate) is false, throw a RangeError exception.
2. If newTarget is not present, set newTarget to %Temporal.PlainMonthDay%.
3. Let object be ? OrdinaryCreateFromConstructor(newTarget, "%Temporal.PlainMonthDay.prototype%", « [[InitializedTemporalMonthDay]], [[ISODate]], [[Calendar]] »).
4. Set object.[[ISODate]] to isoDate.
5. Set object.[[Calendar]] to calendar.
6. Return object.

# 10.5.3 TemporalMonthDayToString ( monthDay, showCalendar )

The abstract operation TemporalMonthDayToString takes arguments monthDay (a Temporal.PlainMonthDay) and showCalendar (one of auto, always, never, or critical) and returns a String. It formats monthDay into an ISO 8601 / RFC 9557 string. It performs the following steps when called:

1. Let month be ToZeroPaddedDecimalString(monthDay.[[ISODate]].[[Month]], 2).
2. Let day be ToZeroPaddedDecimalString(monthDay.[[ISODate]].[[Day]], 2).
3. Let result be the string-concatenation of month, the code unit 0x002D (HYPHEN-MINUS), and day.
4. If showCalendar is one of always or critical, or monthDay.[[Calendar]] is not "iso8601", then
   1. Let year be PadISOYear(monthDay.[[ISODate]].[[Year]]).
   2. Set result to the string-concatenation of year, the code unit 0x002D (HYPHEN-MINUS), and result.
5. Let calendarString be FormatCalendarAnnotation(monthDay.[[Calendar]], showCalendar).
6. Set result to the string-concatenation of result and calendarString.
7. Return result.

# 11 Time Zones

# 11.1 Abstract Operations

Editor's Note

In ECMA-262, many time-zone-related sections and abstract operations are contained in the Date Objects section of the specification. It may be appropriate to move those sections here, for example:

- Time Zone Identifiers
- AvailableNamedTimeZoneIdentifiers
- SystemTimeZoneIdentifier
- IsTimeZoneOffsetString
- GetNamedTimeZoneEpochNanoseconds
- GetNamedTimeZoneOffsetNanoseconds

# 11.1.1 GetAvailableNamedTimeZoneIdentifier ( timeZoneIdentifier )

The abstract operation GetAvailableNamedTimeZoneIdentifier takes argument timeZoneIdentifier (a named time zone identifier) and returns either a Time Zone Identifier Record or empty. If timeZoneIdentifier is an available named time zone identifier, then it returns one of the records in the List returned by AvailableNamedTimeZoneIdentifiers. Otherwise, empty will be returned. It performs the following steps when called:

1. For each element record of AvailableNamedTimeZoneIdentifiers(), do
   1. If record.[[Identifier]] is an ASCII-case-insensitive match for timeZoneIdentifier, return record.
2. Return empty.

Note

For any timeZoneIdentifier, or any value that is an ASCII-case-insensitive match for it, the result of this operation must remain the same for the lifetime of the surrounding agent. Specifically, if that result is a Time Zone Identifier Record, its fields must contain the same values.

Furthermore, time zone identifiers must not dynamically change from primary to non-primary or vice versa during the lifetime of the surrounding agent, meaning that if timeZoneIdentifier is an ASCII-case-insensitive match for the [[PrimaryIdentifier]] field of the result of a previous call to GetAvailableNamedTimeZoneIdentifier, then GetAvailableNamedTimeZoneIdentifier(timeZoneIdentifier) must return a record where [[Identifier]] is [[PrimaryIdentifier]].

Due to the complexity of supporting these requirements, it is recommended that the result of AvailableNamedTimeZoneIdentifiers (and therefore GetAvailableNamedTimeZoneIdentifier) remains the same for the lifetime of the surrounding agent.

# 11.1.2 GetISOPartsFromEpoch ( epochNanoseconds )

The abstract operation GetISOPartsFromEpoch takes argument epochNanoseconds (an integer) and returns an ISO Date-Time Record. It returns the components of a date in UTC corresponding to the given number of nanoseconds since the epoch. It performs the following steps when called:

1. Assert: IsValidEpochNanoseconds(ℤ(epochNanoseconds)) is true.
2. Let remainderNs be epochNanoseconds modulo 106.
3. Let epochMilliseconds be 𝔽((epochNanoseconds \- remainderNs) / 106).
4. Let year be EpochTimeToEpochYear(epochMilliseconds).
5. Let month be EpochTimeToMonthInYear(epochMilliseconds) + 1.
6. Let day be EpochTimeToDate(epochMilliseconds).
7. Let hour be ℝ(HourFromTime(epochMilliseconds)).
8. Let minute be ℝ(MinFromTime(epochMilliseconds)).
9. Let second be ℝ(SecFromTime(epochMilliseconds)).
10. Let millisecond be ℝ(msFromTime(epochMilliseconds)).
11. Let microsecond be floor(remainderNs / 1000).
12. Assert: microsecond < 1000.
13. Let nanosecond be remainderNs modulo 1000.
14. Let isoDate be CreateISODateRecord(year, month, day).
15. Let time be CreateTimeRecord(hour, minute, second, millisecond, microsecond, nanosecond).
16. Return CombineISODateAndTimeRecord(isoDate, time).

# 11.1.3 GetNamedTimeZoneNextTransition ( timeZoneIdentifier, epochNanoseconds )

The implementation-defined abstract operation GetNamedTimeZoneNextTransition takes arguments timeZoneIdentifier (an available named time zone identifier) and epochNanoseconds (a BigInt) and returns either a BigInt or null.

The returned value t represents the number of nanoseconds since the epoch that corresponds to the first time zone UTC offset transition strictly after epochNanoseconds in the IANA time zone identified by timeZoneIdentifier. The operation returns null if no such transition exists for which t ≤ ℤ(nsMaxInstant).

A transition is a point in time where the UTC offset of a time zone changes, for example when Daylight Saving Time starts or stops. The returned value t represents the first nanosecond where the new UTC offset is used in this time zone, not the last nanosecond where the previous UTC offset is used. In other words, GetOffsetNanosecondsFor(timeZone, t) ≠ GetOffsetNanosecondsFor(timeZone, t \- 1).

Given the same values of timeZoneIdentifier and epochNanoseconds, the result must be the same for the lifetime of the surrounding agent.

The minimum implementation of GetNamedTimeZoneNextTransition for ECMAScript implementations that do not include local political rules for any time zones performs the following steps when called:

1. Assert: timeZoneIdentifier is "UTC".
2. Return null.

# 11.1.4 GetNamedTimeZonePreviousTransition ( timeZoneIdentifier, epochNanoseconds )

The implementation-defined abstract operation GetNamedTimeZonePreviousTransition takes arguments timeZoneIdentifier (an available named time zone identifier) and epochNanoseconds (a BigInt) and returns either a BigInt or null.

The returned value t represents the number of nanoseconds since the epoch that corresponds to the last time zone UTC offset transition strictly before epochNanoseconds in the IANA time zone identified by timeZoneIdentifier. The operation returns null if no such transition exists for which t ≥ ℤ(nsMinInstant).

A transition is a point in time where the UTC offset of a time zone changes, for example when Daylight Saving Time starts or stops. The returned value t represents the first nanosecond where the new UTC offset is used in this time zone, not the last nanosecond where the previous UTC offset is used. In other words, GetOffsetNanosecondsFor(timeZone, t) ≠ GetOffsetNanosecondsFor(timeZone, t \- 1).

Given the same values of timeZoneIdentifier and epochNanoseconds, the result must be the same for the lifetime of the surrounding agent.

The minimum implementation of GetNamedTimeZonePreviousTransition for ECMAScript implementations that do not include local political rules for any time zones performs the following steps when called:

1. Assert: timeZoneIdentifier is "UTC".
2. Return null.

# 11.1.5 FormatOffsetTimeZoneIdentifier ( offsetMinutes [ , style ] )

The abstract operation FormatOffsetTimeZoneIdentifier takes argument offsetMinutes (an integer in the inclusive interval from -1439 to 1439) and optional argument style (either separated or unseparated) and returns an offset time zone identifier. It formats a UTC offset, in minutes, into a UTC offset string. If style is separated or not present, then the output will be formatted like ±HH:MM. If style is unseparated, then the output will be formatted like ±HHMM. It performs the following steps when called:

1. If offsetMinutes ≥ 0, let sign be the code unit 0x002B (PLUS SIGN); else let sign be the code unit 0x002D (HYPHEN-MINUS).
2. Let absoluteMinutes be abs(offsetMinutes).
3. Let hour be floor(absoluteMinutes / 60).
4. Let minute be absoluteMinutes modulo 60.
5. Let timeString be FormatTimeString(hour, minute, 0, 0, minute, style).
6. Return the string-concatenation of sign and timeString.

# 11.1.6 FormatUTCOffsetNanoseconds ( offsetNanoseconds )

The abstract operation FormatUTCOffsetNanoseconds takes argument offsetNanoseconds (an integer in the interval from -86400 × 109 (exclusive) to 86400 × 109 (exclusive)) and returns a String. If the offset represents an integer number of minutes, then the output will be formatted like ±HH:MM. Otherwise, the output will be formatted like ±HH:MM:SS or (if the offset does not evenly divide into seconds) ±HH:MM:SS.fff… where the "fff" part is a sequence of at least 1 and at most 9 fractional seconds digits with no trailing zeroes. It performs the following steps when called:

1. If offsetNanoseconds ≥ 0, let sign be the code unit 0x002B (PLUS SIGN); else let sign be the code unit 0x002D (HYPHEN-MINUS).
2. Let absoluteNanoseconds be abs(offsetNanoseconds).
3. Let hour be floor(absoluteNanoseconds / (3600 × 109)).
4. Let minute be floor(absoluteNanoseconds / (60 × 109)) modulo 60.
5. Let second be floor(absoluteNanoseconds / 109) modulo 60.
6. Let subSecondNanoseconds be absoluteNanoseconds modulo 109.
7. If second = 0 and subSecondNanoseconds = 0, let precision be minute; else let precision be auto.
8. Let timeString be FormatTimeString(hour, minute, second, subSecondNanoseconds, precision).
9. Return the string-concatenation of sign and timeString.

# 11.1.7 FormatDateTimeUTCOffsetRounded ( offsetNanoseconds )

The abstract operation FormatDateTimeUTCOffsetRounded takes argument offsetNanoseconds (an integer in the interval from -86400 × 109 (exclusive) to 86400 × 109 (exclusive)) and returns a String. It rounds offsetNanoseconds to the nearest minute boundary and formats the rounded value into a ±HH:MM format, to support available named time zones that may have sub-minute offsets. It performs the following steps when called:

1. Set offsetNanoseconds to RoundNumberToIncrement(offsetNanoseconds, 60 × 109, half-expand).
2. Let offsetMinutes be offsetNanoseconds / (60 × 109).
3. Return FormatOffsetTimeZoneIdentifier(offsetMinutes).

# 11.1.8 ToTemporalTimeZoneIdentifier ( temporalTimeZoneLike )

The abstract operation ToTemporalTimeZoneIdentifier takes argument temporalTimeZoneLike (an ECMAScript value) and returns either a normal completion containing an available time zone identifier or a throw completion. It attempts to derive a value from temporalTimeZoneLike that is an available time zone identifier, and returns that value if found or throws an exception if not. It performs the following steps when called:

1. If temporalTimeZoneLike is an Object and temporalTimeZoneLike has an [[InitializedTemporalZonedDateTime]] internal slot, return temporalTimeZoneLike.[[TimeZone]].
2. If temporalTimeZoneLike is not a String, throw a TypeError exception.
3. Let parseResult be ? ParseTemporalTimeZoneString(temporalTimeZoneLike).
4. Let offsetMinutes be parseResult.[[OffsetMinutes]].
5. If offsetMinutes is not empty, return FormatOffsetTimeZoneIdentifier(offsetMinutes).
6. Let name be parseResult.[[Name]].
7. Let timeZoneIdentifierRecord be GetAvailableNamedTimeZoneIdentifier(name).
8. If timeZoneIdentifierRecord is empty, throw a RangeError exception.
9. Return timeZoneIdentifierRecord.[[Identifier]].

# 11.1.9 GetOffsetNanosecondsFor ( timeZone, epochNs )

The abstract operation GetOffsetNanosecondsFor takes arguments timeZone (an available time zone identifier) and epochNs (a BigInt) and returns an integer in the interval from -86400 × 109 (exclusive) to 86400 × 109 (exclusive). It determines the UTC offset of an exact time epochNs, in nanoseconds. It performs the following steps when called:

1. Let parseResult be ! ParseTimeZoneIdentifier(timeZone).
2. If parseResult.[[OffsetMinutes]] is not empty, return parseResult.[[OffsetMinutes]] × (60 × 109).
3. Return GetNamedTimeZoneOffsetNanoseconds(parseResult.[[Name]], epochNs).

# 11.1.10 GetISODateTimeFor ( timeZone, epochNs )

The abstract operation GetISODateTimeFor takes arguments timeZone (an available time zone identifier) and epochNs (a BigInt) and returns an ISO Date-Time Record. It converts epochNs to a wall-clock date and time in the time zone timeZone. It performs the following steps when called:

1. Let offsetNanoseconds be GetOffsetNanosecondsFor(timeZone, epochNs).
2. Let result be GetISOPartsFromEpoch(ℝ(epochNs)).
3. Return BalanceISODateTime(result.[[ISODate]].[[Year]], result.[[ISODate]].[[Month]], result.[[ISODate]].[[Day]], result.[[Time]].[[Hour]], result.[[Time]].[[Minute]], result.[[Time]].[[Second]], result.[[Time]].[[Millisecond]], result.[[Time]].[[Microsecond]], result.[[Time]].[[Nanosecond]] \+ offsetNanoseconds).

# 11.1.11 GetEpochNanosecondsFor ( timeZone, isoDateTime, disambiguation )

The abstract operation GetEpochNanosecondsFor takes arguments timeZone (an available time zone identifier), isoDateTime (an ISO Date-Time Record), and disambiguation (one of compatible, earlier, later, or reject) and returns either a normal completion containing a BigInt or a throw completion. It converts isoDateTime to an epoch nanoseconds value in the time zone timeZone, using disambiguation to resolve ambiguity. It performs the following steps when called:

1. Let possibleEpochNs be ? GetPossibleEpochNanoseconds(timeZone, isoDateTime).
2. Return ? DisambiguatePossibleEpochNanoseconds(possibleEpochNs, timeZone, isoDateTime, disambiguation).

# 11.1.12 DisambiguatePossibleEpochNanoseconds ( possibleEpochNs, timeZone, isoDateTime, disambiguation )

The abstract operation DisambiguatePossibleEpochNanoseconds takes arguments possibleEpochNs (a List of BigInts), timeZone (an available time zone identifier), isoDateTime (an ISO Date-Time Record), and disambiguation (one of compatible, earlier, later, or reject) and returns either a normal completion containing a BigInt or a throw completion. It chooses from a List of possible exact times the one indicated by the disambiguation parameter. It performs the following steps when called:

1. Let n be the number of elements in possibleEpochNs.
2. If n = 1, return the sole element of possibleEpochNs.
3. If n ≠ 0, then
   1. If disambiguation is either earlier or compatible, return possibleEpochNs[0].
   2. If disambiguation is later, return possibleEpochNs[n \- 1].
   3. Assert: disambiguation is reject.
   4. Throw a RangeError exception.
4. Assert: n = 0.
5. If disambiguation is reject, throw a RangeError exception.
6. Let before be the latest possible ISO Date-Time Record for which CompareISODateTime(before, isoDateTime) = -1 and ! GetPossibleEpochNanoseconds(timeZone, before) is not empty.
7. Let after be the earliest possible ISO Date-Time Record for which CompareISODateTime(after, isoDateTime) = 1 and ! GetPossibleEpochNanoseconds(timeZone, after) is not empty.
8. Let beforePossible be ! GetPossibleEpochNanoseconds(timeZone, before).
9. Assert: The number of elements in beforePossible = 1.
10. Let afterPossible be ! GetPossibleEpochNanoseconds(timeZone, after).
11. Assert: The number of elements in afterPossible = 1.
12. Let offsetBefore be GetOffsetNanosecondsFor(timeZone, the sole element of beforePossible).
13. Let offsetAfter be GetOffsetNanosecondsFor(timeZone, the sole element of afterPossible).
14. Let nanoseconds be offsetAfter \- offsetBefore.
15. Assert: abs(nanoseconds) ≤ nsPerDay.
16. If disambiguation is earlier, then
    1. Let timeDuration be TimeDurationFromComponents(0, 0, 0, 0, 0, -nanoseconds).
    2. Let earlierTime be AddTime(isoDateTime.[[Time]], timeDuration).
    3. Let earlierDate be AddDaysToISODate(isoDateTime.[[ISODate]], earlierTime.[[Days]]).
    4. Let earlierDateTime be CombineISODateAndTimeRecord(earlierDate, earlierTime).
    5. Set possibleEpochNs to ? GetPossibleEpochNanoseconds(timeZone, earlierDateTime).
    6. Assert: possibleEpochNs is not empty.
    7. Return possibleEpochNs[0].
17. Assert: disambiguation is compatible or later.
18. Let timeDuration be TimeDurationFromComponents(0, 0, 0, 0, 0, nanoseconds).
19. Let laterTime be AddTime(isoDateTime.[[Time]], timeDuration).
20. Let laterDate be AddDaysToISODate(isoDateTime.[[ISODate]], laterTime.[[Days]]).
21. Let laterDateTime be CombineISODateAndTimeRecord(laterDate, laterTime).
22. Set possibleEpochNs to ? GetPossibleEpochNanoseconds(timeZone, laterDateTime).
23. Set n to the number of elements in possibleEpochNs.
24. Assert: n ≠ 0.
25. Return possibleEpochNs[n \- 1].

# 11.1.13 GetPossibleEpochNanoseconds ( timeZone, isoDateTime )

The abstract operation GetPossibleEpochNanoseconds takes arguments timeZone (an available time zone identifier) and isoDateTime (an ISO Date-Time Record) and returns either a normal completion containing a List of BigInts or a throw completion. It determines the possible exact times that may correspond to isoDateTime. It performs the following steps when called:

1. Let parseResult be ! ParseTimeZoneIdentifier(timeZone).
2. If parseResult.[[OffsetMinutes]] is not empty, then
   1. Let balanced be BalanceISODateTime(isoDateTime.[[ISODate]].[[Year]], isoDateTime.[[ISODate]].[[Month]], isoDateTime.[[ISODate]].[[Day]], isoDateTime.[[Time]].[[Hour]], isoDateTime.[[Time]].[[Minute]] \- parseResult.[[OffsetMinutes]], isoDateTime.[[Time]].[[Second]], isoDateTime.[[Time]].[[Millisecond]], isoDateTime.[[Time]].[[Microsecond]], isoDateTime.[[Time]].[[Nanosecond]]).
   2. Perform ? CheckISODaysRange(balanced.[[ISODate]]).
   3. Let epochNanoseconds be GetUTCEpochNanoseconds(balanced).
   4. Let possibleEpochNanoseconds be « epochNanoseconds ».
3. Else,
   1. Let possibleEpochNanoseconds be GetNamedTimeZoneEpochNanoseconds(parseResult.[[Name]], isoDateTime).
4. For each value epochNanoseconds in possibleEpochNanoseconds, do
   1. If IsValidEpochNanoseconds(epochNanoseconds) is false, throw a RangeError exception.
5. Return possibleEpochNanoseconds.

# 11.1.14 GetStartOfDay ( timeZone, isoDate )

The abstract operation GetStartOfDay takes arguments timeZone (an available time zone identifier) and isoDate (an ISO Date Record) and returns either a normal completion containing a BigInt or a throw completion. It determines the exact time that corresponds to the first valid wall-clock time in the calendar date isoDate in timeZone. It performs the following steps when called:

1. Let isoDateTime be CombineISODateAndTimeRecord(isoDate, MidnightTimeRecord()).
2. Let possibleEpochNs be ? GetPossibleEpochNanoseconds(timeZone, isoDateTime).
3. If possibleEpochNs is not empty, return possibleEpochNs[0].
4. Assert: IsOffsetTimeZoneIdentifier(timeZone) is false.
5. Let possibleEpochNsAfter be GetNamedTimeZoneEpochNanoseconds(timeZone, isoDateTimeAfter), where isoDateTimeAfter is the ISO Date-Time Record for which DifferenceISODateTime(isoDateTime, isoDateTimeAfter, "iso8601", hour).[[Time]] is the smallest possible value > 0 for which possibleEpochNsAfter is not empty (i.e., isoDateTimeAfter represents the first local time after the transition).
6. Assert: The number of elements in possibleEpochNsAfter = 1.
7. Return the sole element of possibleEpochNsAfter.

# 11.1.15 TimeZoneEquals ( one, two )

The abstract operation TimeZoneEquals takes arguments one (an available time zone identifier) and two (an available time zone identifier) and returns a Boolean. It returns true if its arguments represent time zones using the same identifier. It performs the following steps when called:

1. If one is two, return true.
2. If IsOffsetTimeZoneIdentifier(one) is false and IsOffsetTimeZoneIdentifier(two) is false, then
   1. Let recordOne be GetAvailableNamedTimeZoneIdentifier(one).
   2. Let recordTwo be GetAvailableNamedTimeZoneIdentifier(two).
   3. Assert: recordOne is not empty.
   4. Assert: recordTwo is not empty.
   5. If recordOne.[[PrimaryIdentifier]] is recordTwo.[[PrimaryIdentifier]], return true.
3. Assert: If one and two are both offset time zone identifiers, they do not represent the same number of offset minutes.
4. Return false.

# 11.1.16 ParseTimeZoneIdentifier ( identifier )

The abstract operation ParseTimeZoneIdentifier takes argument identifier (a String) and returns either a normal completion containing a Time Zone Identifier Parse Record, or a throw completion. It parses identifier to determine whether it identifies an offset time zone or named time zone. If identifier is syntactically invalid, a RangeError will be thrown. It performs the following steps when called:

1. Let parseResult be ParseText(StringToCodePoints(identifier), TimeZoneIdentifier).
2. If parseResult is a List of errors, throw a RangeError exception.
3. If parseResult contains a TimeZoneIANAName Parse Node, then
   1. Let name be the source text matched by the TimeZoneIANAName Parse Node contained within parseResult.
   2. NOTE: name is syntactically valid, but does not necessarily conform to IANA Time Zone Database naming guidelines or correspond with an available named time zone identifier.
   3. Return Time Zone Identifier Parse Record { [[Name]]: CodePointsToString(name), [[OffsetMinutes]]: empty }.
4. Assert: parseResult contains a UTCOffset[~SubMinutePrecision] Parse Node.
5. Let offset be the source text matched by the UTCOffset[~SubMinutePrecision] Parse Node contained within parseResult.
6. Let offsetNanoseconds be ! ParseDateTimeUTCOffset(CodePointsToString(offset)).
7. Let offsetMinutes be offsetNanoseconds / (60 × 109).
8. Return Time Zone Identifier Parse Record { [[Name]]: empty, [[OffsetMinutes]]: offsetMinutes }.

# 12 Calendars

# 12.1 Calendar Types

At a minimum, ECMAScript implementations must support a built-in calendar named "iso8601", representing the ISO 8601 calendar. In addition, implementations may support any number of other built-in calendars corresponding with those of the Unicode Common Locale Data Repository (CLDR).

ECMAScript implementations identify built-in calendars using a calendar type as defined by Unicode Technical Standard #35, Part 4, Section 2. Their canonical form is a string containing only Unicode Basic Latin lowercase letters (U+0061 LATIN SMALL LETTER A through U+007A LATIN SMALL LETTER Z, inclusive) and/or digits (U+0030 DIGIT ZERO through U+0039 DIGIT NINE, inclusive), with zero or more medial hyphens (U+002D HYPHEN-MINUS).

# 12.1.1 CanonicalizeCalendar ( id )

The abstract operation CanonicalizeCalendar takes argument id (a String) and returns either a normal completion containing a calendar type, or a throw completion. It returns the canonical form of the calendar type denoted by id, or throws an exception if there is no such built-in calendar. It performs the following steps when called:

1. Let calendars be AvailableCalendars().
2. If calendars does not contain the ASCII-lowercase of id, throw a RangeError exception.
3. Return CanonicalizeUValue("ca", id).

# 12.1.2 AvailableCalendars ( )

The implementation-defined abstract operation AvailableCalendars takes no arguments and returns a List of calendar types. The returned List is sorted according to lexicographic code unit order, and contains unique calendar types in canonical form (12.1) identifying the calendars for which the implementation provides the functionality of Intl.DateTimeFormat objects, including their aliases (e.g., either both or neither of "islamicc" and "islamic-civil"). The List must include "iso8601".

# 12.2 Month Codes

Lunisolar calendars may insert leap months into certain years, in order to reconcile the discrepancy between lunar cycles and the solar year. For this reason, a particular month may not have the same ordinal number every year if a leap month is inserted before it.

A month code is a String that refers uniquely to a particular month, even one that is not present every year. Month codes are lexicographically ordered according to the notional order of months in the year, even though not all may be present in any given year. They conform to the following string format:

The month code for a month that is not a leap month and whose 1-based ordinal position in a common year of the calendar (i.e., a year that is not a leap year) is n is the string-concatenation of "M" and ToZeroPaddedDecimalString(n, 2). The month code for a leap month inserted after a month whose 1-based ordinal position in a common year of the calendar is p, is the string-concatenation of "M", ToZeroPaddedDecimalString(p, 2), and "L".

The month codes in the ISO 8601 calendar, which does not have leap months, are "M01" for January through "M12" for December.

Note

For example, in the Hebrew calendar, the month code of Adar (and Adar II, in leap years) is "M06" and the month code of Adar I (the leap month inserted before Adar II) is "M05L". Theoretically, in a calendar with a leap month at the start of some years, the month code of that month would be "M00L".

# 12.2.1 ParseMonthCode ( argument )

The abstract operation ParseMonthCode takes argument argument (an ECMAScript language value) and returns either a normal completion containing a Record with fields [[MonthNumber]] (an integer) and [[IsLeapMonth]] (a Boolean) or a throw completion. It converts argument to a month code and parses it into its parts, or throws a TypeError if conversion to String fails, or throws a RangeError if the result is not a syntactically valid month code. The month code is not guaranteed to be correct in the context of any particular calendar; for example, some calendars do not have leap months.

It performs the following steps when called:

1. Let monthCode be ? ToPrimitive(argument, string).
2. If monthCode is not a String, throw a TypeError exception.
3. If ParseText(StringToCodePoints(monthCode), MonthCode) is a List of errors, throw a RangeError exception.
4. Let isLeapMonth be false.
5. If the length of monthCode = 4, then
   1. Assert: The fourth code unit of monthCode is 0x004C (LATIN CAPITAL LETTER L).
   2. Set isLeapMonth to true.
6. Let monthCodeDigits be the substring of monthCode from 1 to 3.
7. Let monthNumber be ℝ(StringToNumber(monthCodeDigits)).
8. Return the Record { [[MonthNumber]]: monthNumber, [[IsLeapMonth]]: isLeapMonth }.

MonthCode ::: M00L M0 NonZeroDigit Lopt M NonZeroDigit DecimalDigit Lopt

# 12.2.2 CreateMonthCode ( monthNumber, isLeapMonth )

The abstract operation CreateMonthCode takes arguments monthNumber (an integer in the inclusive interval from 0 to 99) and isLeapMonth (a Boolean) and returns a month code. It creates a month code with the given month number and leap month flag.

It performs the following steps when called:

1. Assert: If isLeapMonth is false, monthNumber > 0.
2. Let numberPart be ToZeroPaddedDecimalString(monthNumber, 2).
3. If isLeapMonth is true, then
   1. Return the string-concatenation of the code unit 0x004D (LATIN CAPITAL LETTER M), numberPart, and the code unit 0x004C (LATIN CAPITAL LETTER L).
4. Return the string-concatenation of the code unit 0x004D (LATIN CAPITAL LETTER M) and numberPart.

# 12.3 Abstract Operations

# 12.3.1 Calendar Date Records

A Calendar Date Record is a Record value used to represent a valid calendar date in a non-ISO 8601 calendar. Calendar Date Records are produced by the abstract operation CalendarISOToDate.

Calendar Date Records have the fields listed in Table 18.

| Table 18: Calendar Date Record Fields Field Name | Value                   | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| ------------------------------------------------ | ----------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [[Era]]                                          | a String or undefined   | A lowercase String representing the date's era, or undefined for calendars that do not have eras.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| [[EraYear]]                                      | an integer or undefined | The ordinal position of the date's year within its era, or undefined for calendars that do not have eras. Note 1 Era years are 1-indexed for many calendars, but not all (e.g., the eras of the Burmese calendar, not currently available in CLDR, each start with a year 0). Years can also advance opposite the flow of time (as for BCE years in the Gregorian calendar).                                                                                                                                                                                                                                                                                                                                                                                                |
| [[Year]]                                         | an integer              | The date's year relative to the first day of a calendar-specific "epoch year". Note 2The year is relative to the first day of the calendar's epoch year, so if the epoch era starts in the middle of the year, the year will be the same value before and after the start date of the era.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| [[Month]]                                        | a positive integer      | The 1-based ordinal position of the date's month within its year. Note 3 When the number of months in a year of the calendar is variable, a different value can be returned for dates that are part of the same month in different years. For example, in the Hebrew calendar, 1 Nisan 5781 is associated with value 7 while 1 Nisan 5782 is associated with value 8 because 5782 is a leap year and Nisan follows the insertion of Adar I.                                                                                                                                                                                                                                                                                                                                 |
| [[MonthCode]]                                    | a month code            | The month code of the date's month.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| [[Day]]                                          | a positive integer      | The 1-based ordinal position of the date's day within its month.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| [[DayOfWeek]]                                    | a positive integer      | The day of the week corresponding to the date. The value should be 1-based, where 1 is the day corresponding to Monday in the given calendar.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| [[DayOfYear]]                                    | a positive integer      | The 1-based ordinal position of the date's day within its year.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| [[WeekOfYear]]                                   | a Year-Week Record      | The date's _calendar week of year_ , and the corresponding _week calendar year_. The Year-Week Record's [[Week]] field should be 1-based. The Year-Week Record's [[Year]] field is relative to the first day of a calendar-specific "epoch year", as in the Calendar Date Record's [[Year]] field, not relative to an era as in [[EraYear]]. Usually the Year-Week Record's [[Year]] field will contain the same value as the Calendar Date Record's [[Year]] field, but may contain the previous or next year if the week number in the Year-Week Record's [[Week]] field overlaps two different years. See also ISOWeekOfYear. The Year-Week Record contains undefined in [[Week]] and [[Year]] field for calendars that do not have a well-defined week calendar system. |
| [[DaysInWeek]]                                   | a positive integer      | The number of days in the date's week.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| [[DaysInMonth]]                                  | a positive integer      | The number of days in the date's month.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| [[DaysInYear]]                                   | a positive integer      | The number of days in the date's year.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| [[MonthsInYear]]                                 | a positive integer      | The number of months in the date's year.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| [[InLeapYear]]                                   | a Boolean               | true if the date falls within a leap year, and false otherwise. Note 4 A "leap year" is a year that contains more days than other years (for solar or lunar calendars) or more months than other years (for lunisolar calendars like Hebrew or Chinese). Some calendars, especially lunisolar ones, have further variation in year length that is not represented in the output of this operation (e.g., the Hebrew calendar includes common years with 353, 354, or 355 days and leap years with 383, 384, or 385 days).                                                                                                                                                                                                                                                   |

# 12.3.2 Calendar Fields Records

A Calendar Fields Record is a Record value used to represent full or partial input for a calendar date in a non-ISO 8601 calendar. Calendar Fields Records are produced by several abstract operations, such as ISODateToFields and PrepareCalendarFields, and are passed to abstract operations such as CalendarDateFromFields.

Many of the fields in a Calendar Fields Record have the same meaning as the fields of the same name in Calendar Date Records, but each field in a Calendar Fields Record may additionally be unset to indicate partial input.

Each field has a corresponding property key. These property keys correspond to the properties that are read from user input objects to populate the field, in methods such as `Temporal.PlainDate.prototype.with()`.

Each field has a corresponding enumeration key. Lists of these enumeration keys are passed to abstract operations such as PrepareCalendarFields and CalendarFieldKeysToIgnore to denote a subset of fields to operate on.

Calendar Fields Records have the fields listed in Table 19.

| Table 19: Calendar Fields Record Fields Field Name | Value                                                       | Default | Property Key  | Enumeration Key | Conversion                          | Meaning                                                                                    |
| -------------------------------------------------- | ----------------------------------------------------------- | ------- | ------------- | --------------- | ----------------------------------- | ------------------------------------------------------------------------------------------ |
| [[Era]]                                            | a String or unset                                           | unset   | "era"         | era             | to-string                           | A lowercase String representing the era.                                                   |
| [[EraYear]]                                        | an integer or unset                                         | unset   | "eraYear"     | era-year        | to-integer-with-truncation          | The ordinal position of the year within the era.                                           |
| [[Year]]                                           | an integer or unset                                         | unset   | "year"        | year            | to-integer-with-truncation          | The year relative to the first day of a calendar-specific "epoch year".                    |
| [[Month]]                                          | a positive integer or unset                                 | unset   | "month"       | month           | to-positive-integer-with-truncation | The 1-based ordinal position of the month within the year.                                 |
| [[MonthCode]]                                      | a month code or unset                                       | unset   | "monthCode"   | month-code      | to-month-code                       | The month code of the month.                                                               |
| [[Day]]                                            | a positive integer or unset                                 | unset   | "day"         | day             | to-positive-integer-with-truncation | The 1-based ordinal position of the day within the month.                                  |
| [[Hour]]                                           | an integer in the inclusive interval from 0 to 23 or unset  | 0       | "hour"        | hour            | to-integer-with-truncation          | The number of the hour within the day.                                                     |
| [[Minute]]                                         | an integer in the inclusive interval from 0 to 59 or unset  | 0       | "minute"      | minute          | to-integer-with-truncation          | The number of the minute within the hour.                                                  |
| [[Second]]                                         | an integer in the inclusive interval from 0 to 59 or unset  | 0       | "second"      | second          | to-integer-with-truncation          | The number of the second within the minute.                                                |
| [[Millisecond]]                                    | an integer in the inclusive interval from 0 to 999 or unset | 0       | "millisecond" | millisecond     | to-integer-with-truncation          | The number of the millisecond within the second.                                           |
| [[Microsecond]]                                    | an integer in the inclusive interval from 0 to 999 or unset | 0       | "microsecond" | microsecond     | to-integer-with-truncation          | The number of the microsecond within the millisecond.                                      |
| [[Nanosecond]]                                     | an integer in the inclusive interval from 0 to 999 or unset | 0       | "nanosecond"  | nanosecond      | to-integer-with-truncation          | The number of the nanosecond within the microsecond.                                       |
| [[OffsetString]]                                   | a String or unset                                           | unset   | "offset"      | offset          | to-offset-string                    | A string of the form `±HH:MM[:SS.SSSSSSSSS]` that can be parsed by ParseDateTimeUTCOffset. |
| [[TimeZone]]                                       | a String or unset                                           | unset   | "timeZone"    | time-zone       | to-temporal-time-zone-identifier    | An available time zone identifier.                                                         |

# 12.3.3 PrepareCalendarFields ( calendar, fields, calendarFieldNames, nonCalendarFieldNames, requiredFieldNames )

The abstract operation PrepareCalendarFields takes arguments calendar (a calendar type), fields (an Object), calendarFieldNames (a List of values from the Enumeration Key column of Table 19), nonCalendarFieldNames (a List of values from the Enumeration Key column of Table 19), and requiredFieldNames (either a List of values from the Enumeration Key column of Table 19 or partial) and returns either a normal completion containing a Calendar Fields Record, or a throw completion. It returns the result of reading from fields all of the property names corresponding to calendarFieldNames and nonCalendarFieldNames, plus any extra fields required by the calendar. The returned Record has a non-unset value for each element of fieldNames that corresponds with a non-undefined property on fields used as the input for relevant conversion. When requiredFields is partial, this operation throws if none of the properties are present with a non-undefined value. When requiredFields is a List, this operation throws if any of the properties named by it are absent or undefined. It performs the following steps when called:

1. Assert: If requiredFieldNames is a List, requiredFieldNames contains zero or one of each of the elements of calendarFieldNames and nonCalendarFieldNames.
2. Let fieldNames be the list-concatenation of calendarFieldNames and nonCalendarFieldNames.
3. Let extraFieldNames be CalendarExtraFields(calendar, calendarFieldNames).
4. Set fieldNames to the list-concatenation of fieldNames and extraFieldNames.
5. Assert: fieldNames contains no duplicate elements.
6. Let result be a Calendar Fields Record with all fields equal to unset.
7. Let any be false.
8. Let sortedPropertyNames be a List whose elements are the values in the Property Key column of Table 19 corresponding to the elements of fieldNames, sorted according to lexicographic code unit order.
9. For each property name property of sortedPropertyNames, do
   1. Let key be the value in the Enumeration Key column of Table 19 corresponding to the row whose Property Key value is property.
   2. Let value be ? Get(fields, property).
   3. If value is not undefined, then
      1. Set any to true.
      2. Let Conversion be the Conversion value of the same row.
      3. If Conversion is to-integer-with-truncation, then
         1. Set value to ? ToIntegerWithTruncation(value).
         2. Set value to 𝔽(value).
      4. Else if Conversion is to-positive-integer-with-truncation, then
         1. Set value to ? ToPositiveIntegerWithTruncation(value).
         2. Set value to 𝔽(value).
      5. Else if Conversion is to-string, then
         1. Set value to ? ToString(value).
      6. Else if Conversion is to-temporal-time-zone-identifier, then
         1. Set value to ? ToTemporalTimeZoneIdentifier(value).
      7. Else if Conversion is to-month-code, then
         1. Let parsed be ? ParseMonthCode(value).
         2. Set value to CreateMonthCode(parsed.[[MonthNumber]], parsed.[[IsLeapMonth]]).
      8. Else,
         1. Assert: Conversion is to-offset-string.
         2. Set value to ? ToOffsetString(value).
      9. Set result's field whose name is given in the Field Name column of the same row to value.
   4. Else if requiredFieldNames is a List, then
      1. If requiredFieldNames contains key, throw a TypeError exception.
      2. Set result's field whose name is given in the Field Name column of the same row to the corresponding Default value of the same row.
10. If requiredFieldNames is partial and any is false, throw a TypeError exception.
11. Return result.

# 12.3.4 CalendarFieldKeysPresent ( fields )

The abstract operation CalendarFieldKeysPresent takes argument fields (a Calendar Fields Record) and returns a List of values from the Enumeration Key column of Table 19. The returned List contains the enumeration keys for all fields of fields that are not unset. It performs the following steps when called:

1. Let list be « ».
2. For each row of Table 19, except the header row, do
   1. Let value be fields' field whose name is given in the Field Name column of the row.
   2. Let enumerationKey be the value in the Enumeration Key column of the row.
   3. If value is not unset, append enumerationKey to list.
3. Return list.

# 12.3.5 CalendarMergeFields ( calendar, fields, additionalFields )

The abstract operation CalendarMergeFields takes arguments calendar (a calendar type), fields (a Calendar Fields Record), and additionalFields (a Calendar Fields Record) and returns a Calendar Fields Record. It merges the properties of fields and additionalFields. It performs the following steps when called:

1. Let additionalKeys be CalendarFieldKeysPresent(additionalFields).
2. Let overriddenKeys be CalendarFieldKeysToIgnore(calendar, additionalKeys).
3. Let merged be a Calendar Fields Record with all fields set to unset.
4. Let fieldsKeys be CalendarFieldKeysPresent(fields).
5. For each row of Table 19, except the header row, do
   1. Let key be the value in the Enumeration Key column of the row.
   2. If fieldsKeys contains key and overriddenKeys does not contain key, then
      1. Let propValue be fields' field whose name is given in the Field Name column of the row.
      2. Set merged's field whose name is given in the Field Name column of the row to propValue.
   3. If additionalKeys contains key, then
      1. Let propValue be additionalFields' field whose name is given in the Field Name column of the row.
      2. Set merged's field whose name is given in the Field Name column of the row to propValue.
6. Return merged.

# 12.3.6 NonISODateAdd ( calendar, isoDate, duration, overflow )

The implementation-defined abstract operation NonISODateAdd takes arguments calendar (a calendar type that is not "iso8601"), isoDate (an ISO Date Record), duration (a Date Duration Record), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date Record or a throw completion. The operation performs implementation-defined processing to add duration to date in the context of the calendar represented by calendar and returns the corresponding day, month and year of the result in the ISO 8601 calendar values as an ISO Date Record. It may throw a RangeError exception if overflow is reject and the resulting month or day would need to be clamped in order to form a valid date in calendar.

# 12.3.7 CalendarDateAdd ( calendar, isoDate, duration, overflow )

The abstract operation CalendarDateAdd takes arguments calendar (a calendar type), isoDate (an ISO Date Record), duration (a Date Duration Record), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date Record or a throw completion. It adds dateDuration to isoDate using the years, months, and weeks reckoning of calendar. If addition of years or months results in a nonexistent date, depending on overflow it will be coerced to an existing date or the operation will throw. It performs the following steps when called:

1. If calendar is "iso8601", then
   1. Let intermediate be BalanceISOYearMonth(isoDate.[[Year]] \+ duration.[[Years]], isoDate.[[Month]] \+ duration.[[Months]]).
   2. Set intermediate to ? RegulateISODate(intermediate.[[Year]], intermediate.[[Month]], isoDate.[[Day]], overflow).
   3. Let days be duration.[[Days]] \+ 7 × duration.[[Weeks]].
   4. Let result be AddDaysToISODate(intermediate, days).
2. Else,
   1. Let result be ? NonISODateAdd(calendar, isoDate, duration, overflow).
3. If ISODateWithinLimits(result) is false, throw a RangeError exception.
4. Return result.

# 12.3.8 NonISODateUntil ( calendar, one, two, largestUnit )

The implementation-defined abstract operation NonISODateUntil takes arguments calendar (a calendar type that is not "iso8601"), one (an ISO Date Record), two (an ISO Date Record), and largestUnit (a date unit) and returns a Date Duration Record. It performs implementation-defined processing to determine the difference between the dates one and two using the years, months, and weeks reckoning of calendar. No fields larger than largestUnit will be non-zero in the resulting Date Duration Record.

# 12.3.9 CalendarDateUntil ( calendar, one, two, largestUnit )

The abstract operation CalendarDateUntil takes arguments calendar (a calendar type), one (an ISO Date Record), two (an ISO Date Record), and largestUnit (a date unit) and returns a Date Duration Record. It determines the difference between the dates one and two using the years, months, and weeks reckoning of calendar. No fields larger than largestUnit will be non-zero in the resulting Date Duration Record. It performs the following steps when called:

1. Let sign be CompareISODate(one, two).
2. If sign = 0, return ZeroDateDuration().
3. If calendar is "iso8601", then
   1. Set sign to -sign.
   2. Let years be 0.
   3. If largestUnit is year, then
      1. Let candidateYears be sign.
      2. Repeat, while ISODateSurpasses(sign, one, two, candidateYears, 0, 0, 0) is false,
         1. Set years to candidateYears.
         2. Set candidateYears to candidateYears \+ sign.
   4. Let months be 0.
   5. If largestUnit is either year or month, then
      1. Let candidateMonths be sign.
      2. Repeat, while ISODateSurpasses(sign, one, two, years, candidateMonths, 0, 0) is false,
         1. Set months to candidateMonths.
         2. Set candidateMonths to candidateMonths \+ sign.
   6. Let weeks be 0.
   7. If largestUnit is week, then
      1. Let candidateWeeks be sign.
      2. Repeat, while ISODateSurpasses(sign, one, two, years, months, candidateWeeks, 0) is false,
         1. Set weeks to candidateWeeks.
         2. Set candidateWeeks to candidateWeeks \+ sign.
   8. Let days be 0.
   9. Let candidateDays be sign.
   10. Repeat, while ISODateSurpasses(sign, one, two, years, months, weeks, candidateDays) is false,
   11. Set days to candidateDays.
   12. Set candidateDays to candidateDays \+ sign.
   13. Return ! CreateDateDurationRecord(years, months, weeks, days).
4. Return NonISODateUntil(calendar, one, two, largestUnit).

# 12.3.10 ToTemporalCalendarIdentifier ( temporalCalendarLike )

The abstract operation ToTemporalCalendarIdentifier takes argument temporalCalendarLike (an ECMAScript value) and returns either a normal completion containing a calendar type or a throw completion. It attempts to derive a calendar type from temporalCalendarLike, and returns that value if found or throws an exception if not. It performs the following steps when called:

1. If temporalCalendarLike is an Object and temporalCalendarLike has an [[InitializedTemporalDate]], [[InitializedTemporalDateTime]], [[InitializedTemporalMonthDay]], [[InitializedTemporalYearMonth]], or [[InitializedTemporalZonedDateTime]] internal slot, return temporalCalendarLike.[[Calendar]].
2. If temporalCalendarLike is not a String, throw a TypeError exception.
3. Let identifier be ? ParseTemporalCalendarString(temporalCalendarLike).
4. Return ? CanonicalizeCalendar(identifier).

# 12.3.11 GetTemporalCalendarIdentifierWithISODefault ( item )

The abstract operation GetTemporalCalendarIdentifierWithISODefault takes argument item (an Object) and returns either a normal completion containing a calendar type or a throw completion. It looks for a `calendar` property on the given item and converts its value into a calendar identifier. If no such property is present, the built-in ISO 8601 calendar is returned. It performs the following steps when called:

1. If item has an [[InitializedTemporalDate]], [[InitializedTemporalDateTime]], [[InitializedTemporalMonthDay]], [[InitializedTemporalYearMonth]], or [[InitializedTemporalZonedDateTime]] internal slot, then
   1. Return item.[[Calendar]].
2. Let calendarLike be ? Get(item, "calendar").
3. If calendarLike is undefined, return "iso8601".
4. Return ? ToTemporalCalendarIdentifier(calendarLike).

# 12.3.12 CalendarDateFromFields ( calendar, fields, overflow )

The abstract operation CalendarDateFromFields takes arguments calendar (a calendar type), fields (a Calendar Fields Record), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date Record or a throw completion. It converts a calendar date in the reckoning of calendar, if it is uniquely determined by the fields of fields, into an ISO Date Record. It performs the following steps when called:

1. Perform ? CalendarResolveFields(calendar, fields, date).
2. Let result be ? CalendarDateToISO(calendar, fields, overflow).
3. If ISODateWithinLimits(result) is false, throw a RangeError exception.
4. Return result.

# 12.3.13 CalendarYearMonthFromFields ( calendar, fields, overflow )

The abstract operation CalendarYearMonthFromFields takes arguments calendar (a calendar type), fields (a Calendar Fields Record), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date Record or a throw completion. It converts a calendar month in the reckoning of calendar, if it is uniquely determined by the properties on fields, into an ISO Date Record denoting the first day of that month. It performs the following steps when called:

1. Set fields.[[Day]] to 1.
2. Perform ? CalendarResolveFields(calendar, fields, year-month).
3. Let result be ? CalendarDateToISO(calendar, fields, overflow).
4. If ISOYearMonthWithinLimits(result) is false, throw a RangeError exception.
5. Return result.

# 12.3.14 CalendarMonthDayFromFields ( calendar, fields, overflow )

The abstract operation CalendarMonthDayFromFields takes arguments calendar (a calendar type), fields (a Calendar Fields Record), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date Record or a throw completion. It converts a calendar month-day in the reckoning of calendar, if it is uniquely determined by the properties on fields, into an ISO Date Record denoting that day in an appropriate reference year. It performs the following steps when called:

1. Perform ? CalendarResolveFields(calendar, fields, month-day).
2. Let result be ? CalendarMonthDayToISOReferenceDate(calendar, fields, overflow).
3. Assert: ISODateWithinLimits(result) is true.
4. Return result.

Note

The ISODateWithinLimits assertion holds because the result of CalendarMonthDayToISOReferenceDate should return a reference year that falls within the representable range.

# 12.3.15 FormatCalendarAnnotation ( id, showCalendar )

The abstract operation FormatCalendarAnnotation takes arguments id (a calendar type) and showCalendar (one of auto, always, never, or critical) and returns a String. It returns a string with a calendar annotation suitable for concatenating to the end of an ISO 8601 / RFC 9557 string. Depending on the given id and value of showCalendar, the string may be empty if no calendar annotation need be included. It performs the following steps when called:

1. If showCalendar is never, return the empty String.
2. If showCalendar is auto and id is "iso8601", return the empty String.
3. If showCalendar is critical, let flag be "!"; else, let flag be the empty String.
4. Return the string-concatenation of "[", flag, "u-ca=", id, and "]".

# 12.3.16 CalendarEquals ( one, two )

The abstract operation CalendarEquals takes arguments one (a calendar type) and two (a calendar type) and returns a Boolean. It returns true if its arguments represent calendars using the same identifier. It performs the following steps when called:

1. If CanonicalizeUValue("ca", one) is CanonicalizeUValue("ca", two), return true.
2. Return false.

# 12.3.17 ISODaysInMonth ( year, month )

The abstract operation ISODaysInMonth takes arguments year (an integer) and month (an integer in the inclusive interval from 1 to 12) and returns a positive integer. It returns the number of days in the given year and month in the ISO 8601 calendar. It performs the following steps when called:

1. If month is one of 1, 3, 5, 7, 8, 10, or 12, return 31.
2. If month is one of 4, 6, 9, or 11, return 30.
3. Assert: month = 2.
4. Return 28 + MathematicalInLeapYear(EpochTimeForYear(year)).

# 12.3.18 ISOWeekOfYear ( isoDate )

The abstract operation ISOWeekOfYear takes argument isoDate (an ISO Date Record) and returns a Year-Week Record. It determines where a calendar day falls in the ISO 8601 week calendar and calculates its _calendar week of year_ , which is the 1-based ordinal number of its calendar week within the corresponding _week calendar year_ (which may differ from year by up to 1 in either direction). It performs the following steps when called:

1. Let year be isoDate.[[Year]].
2. Let wednesday be 3.
3. Let thursday be 4.
4. Let friday be 5.
5. Let saturday be 6.
6. Let daysInWeek be 7.
7. Let maxWeekNumber be 53.
8. Let dayOfYear be ISODayOfYear(isoDate).
9. Let dayOfWeek be ISODayOfWeek(isoDate).
10. Let week be floor((dayOfYear \+ daysInWeek \- dayOfWeek \+ wednesday) / daysInWeek).
11. If week < 1, then
    1. NOTE: This is the last week of the previous year.
    2. Let jan1st be CreateISODateRecord(year, 1, 1).
    3. Let dayOfJan1st be ISODayOfWeek(jan1st).
    4. If dayOfJan1st = friday, then
    5. Return Year-Week Record { [[Week]]: maxWeekNumber, [[Year]]: year \- 1 }.
       5. If dayOfJan1st = saturday, and MathematicalInLeapYear(EpochTimeForYear(year \- 1)) = 1, then
    6. Return Year-Week Record { [[Week]]: maxWeekNumber. [[Year]]: year \- 1 }.
       6. Return Year-Week Record { [[Week]]: maxWeekNumber \- 1, [[Year]]: year \- 1 }.
12. If week = maxWeekNumber, then
    1. Let daysInYear be MathematicalDaysInYear(year).
    2. Let daysLaterInYear be daysInYear \- dayOfYear.
    3. Let daysAfterThursday be thursday \- dayOfWeek.
    4. If daysLaterInYear < daysAfterThursday, then
    5. Return Year-Week Record { [[Week]]: 1, [[Year]]: year \+ 1 }.
13. Return Year-Week Record { [[Week]]: week, [[Year]]: year }.

Note 1

In the ISO 8601 week calendar, calendar week number 1 of a calendar year is the week including the first Thursday of that year (based on the principle that a week belongs to the same calendar year as the majority of its calendar days), which always includes January 4 and starts on the Monday on or immediately before then. Because of this, some calendar days of the first calendar week of a calendar year may be part of the preceding [proleptic Gregorian] date calendar year, and some calendar days of the last calendar week of a calendar year may be part of the following [proleptic Gregorian] date calendar year. See ISO 8601 for details.

Note 2

For example, week calendar year 2020 includes both 31 December 2019 (a Tuesday belonging to its calendar week 1) and 1 January 2021 (a Friday belonging to its calendar week 53).

# 12.3.19 ISODayOfYear ( isoDate )

The abstract operation ISODayOfYear takes argument isoDate (an ISO Date Record) and returns an integer. It returns the ISO 8601 _calendar day of year_ of a calendar day, which is its 1-based ordinal number within its ISO 8601 calendar year. It performs the following steps when called:

1. Let epochDays be ISODateToEpochDays(isoDate.[[Year]], isoDate.[[Month]] \- 1, isoDate.[[Day]]).
2. Return EpochTimeToDayInYear(EpochDaysToEpochMs(epochDays, 0)) + 1.

# 12.3.20 ISODayOfWeek ( isoDate )

The abstract operation ISODayOfWeek takes argument isoDate (an ISO Date Record) and returns an integer. It returns the ISO 8601 _calendar day of week_ of a calendar day, which is its 1-based ordinal position within the sequence of week calendar days that starts with Monday at 1 and ends with Sunday at 7. It performs the following steps when called:

1. Let epochDays be ISODateToEpochDays(isoDate.[[Year]], isoDate.[[Month]] \- 1, isoDate.[[Day]]).
2. Let dayOfWeek be EpochTimeToWeekDay(EpochDaysToEpochMs(epochDays, 0)).
3. If dayOfWeek = 0, return 7.
4. Return dayOfWeek.

# 12.3.21 NonISOCalendarDateToISO ( calendar, fields, overflow )

The implementation-defined abstract operation NonISOCalendarDateToISO takes arguments calendar (a calendar type that is not "iso8601"), fields (a Calendar Fields Record), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date Record or a throw completion. It performs implementation-defined processing to convert fields, which represents either a date or a year and month in the built-in calendar identified by calendar, to a corresponding representative date in the ISO 8601 calendar, subject to overflow correction specified by overflow. For reject, values that do not form a valid date cause an exception to be thrown, as described below. For constrain, values that do not form a valid date are clamped to their respective valid range.

Like RegulateISODate, the operation throws a RangeError exception when overflow is reject and the date described by fields does not exist.

Clamping an invalid date to the correct range when overflow is constrain is a behaviour specific to each calendar other than "iso8601", but all calendars follow this guideline:

- Pick the closest day in the same month. If there are two equally-close dates in that month, pick the later one.
- If the month is a leap month that doesn't exist in the year, pick another date according to the cultural conventions of that calendar's users. Usually this will result in the same day in the month before or after where that month would normally fall in a leap year.
- Otherwise, pick the closest date that is still in the same year. If there are two equally-close dates in that year, pick the later one.
- If the entire year doesn't exist, pick the closest date in a different year. If there are two equally-close dates, pick the later one.

# 12.3.22 CalendarDateToISO ( calendar, fields, overflow )

The abstract operation CalendarDateToISO takes arguments calendar (a calendar type), fields (a Calendar Fields Record), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date Record or a throw completion. It converts fields, which represents either a date or a year and month in the built-in calendar identified by calendar, to a corresponding representative date in the ISO 8601 calendar, subject to overflow correction specified by overflow. For reject, values that do not form a valid date cause an exception to be thrown, as described below. For constrain, values that do not form a valid date are clamped to their respective valid range. It performs the following steps when called:

1. If calendar is "iso8601", then
   1. Assert: fields.[[Year]], fields.[[Month]], and fields.[[Day]] are not unset.
   2. Return ? RegulateISODate(fields.[[Year]], fields.[[Month]], fields.[[Day]], overflow).
2. Return ? NonISOCalendarDateToISO(calendar, fields, overflow).

# 12.3.23 NonISOMonthDayToISOReferenceDate ( calendar, fields, overflow )

The implementation-defined abstract operation NonISOMonthDayToISOReferenceDate takes arguments calendar (a calendar type that is not "iso8601"), fields (a Calendar Fields Record), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date Record or a throw completion. It performs implementation-defined processing to convert fields, which represents a calendar date without a year (i.e., month code and day pair, or equivalent) in the built-in calendar identified by calendar, to a corresponding reference date in the ISO 8601 calendar as described below, subject to overflow correction specified by overflow. For reject, values that do not form a valid date cause an exception to be thrown. For constrain, values that do not form a valid date are clamped to their respective valid range as in CalendarDateToISO.

The fields of the returned ISO Date Record represent a reference date in the ISO 8601 calendar that, when converted to calendar, corresponds to the month code and day of fields in an arbitrary but deterministically chosen reference year. The reference date is the latest ISO 8601 date corresponding to the calendar date that is between January 1, 1900 and December 31, 1972 inclusive. If there is no such date, it is the earliest ISO 8601 date corresponding to the calendar date between January 1, 1973 and December 31, 2035 inclusive.

The reference year is almost always 1972 (the first ISO 8601 leap year after the epoch), with exceptions for calendars where some dates (e.g. leap days or days in leap months) didn't occur during that ISO 8601 year. For example, Hebrew calendar leap month Adar I occurred in calendar years 5730 and 5733 (respectively overlapping ISO 8601 February/March 1970 and February/March 1973), but did not occur between them, so the reference year for days of that month is 1970.

Like RegulateISODate, the operation throws a RangeError exception if overflow is reject and the month and day described by fields does not exist (or does not exist within the year described by fields when there is such a year). For example, when calendar is "gregory" and overflow is reject, fields values of { [[MonthCode]]: "M01", [[Day]]: 32 } and { [[Year]]: 2001, [[Month]]: 2, [[Day]]: 29 } would both cause a RangeError to be thrown. In the latter case, even though February 29 is a date in leap years of the Gregorian calendar, 2001 was not a leap year and a month code cannot be determined from the nonexistent date 2001-02-29 with the specified month index.

Also like RegulateISODate, if overflow is constrain and the date or month and day described by fields does not exist (or does not exist within the year described by fields when there is such a year), the values of those fields are clamped to their respective valid range. As in CalendarDateToISO, such clamping is calendar-specific. For example, when calendar is "gregory" and overflow is constrain, a fields value of { [[MonthCode]]: "M02", [[Day]]: 30 } is clamped to { [[Year]]: 1972, [[Month]]: 2, [[Day]]: 29 } (because 29 is the maximum valid day for February) while fields values of { [[Year]]: 2001, [[MonthCode]]: "M02", [[Day]]: 30 } and { [[Era]]: "ce", [[EraYear]]: 2001, [[Month]]: 2, [[Day]]: 30 } (or an equivalent with a supported value for [[Era]] representing the Common Era beginning in ISO 8601 year 1) are clamped to { [[Year]]: 1972, [[Month]]: 2, [[Day]]: 28 } (because 28 is the maximum valid day for February 2001, which did not include a leap day).

The operation throws a RangeError if fields.[[Year]] is not unset and the ISO 8601 year year corresponding to fields.[[Year]] would cause ISODateWithinLimits to return false (i.e., year is not in the inclusive interval from -271,821 to 275,760.) This is so as not to require calculating whether the month and day described in fields exist in years arbitrarily far in the future or past. Note this restriction does not apply to CalendarMonthDayToISOReferenceDate when calendar is "iso8601".

# 12.3.24 CalendarMonthDayToISOReferenceDate ( calendar, fields, overflow )

The abstract operation CalendarMonthDayToISOReferenceDate takes arguments calendar (a calendar type), fields (a Calendar Fields Record), and overflow (either constrain or reject) and returns either a normal completion containing an ISO Date Record or a throw completion. It converts fields, which represents a calendar date without a year (i.e., month code and day pair, or equivalent) in the built-in calendar identified by calendar, to a corresponding reference date in the ISO 8601 calendar as described below, subject to overflow correction specified by overflow. For reject, values that do not form a valid date cause an exception to be thrown. For constrain, values that do not form a valid date are clamped to their respective valid range as in CalendarDateToISO. It performs the following steps when called:

1. If calendar is "iso8601", then
   1. Assert: fields.[[Month]] and fields.[[Day]] are not unset.
   2. Let referenceISOYear be 1972 (the first ISO 8601 leap year after the epoch).
   3. If fields.[[Year]] is unset, let year be referenceISOYear; else let year be fields.[[Year]].
   4. Let result be ? RegulateISODate(year, fields.[[Month]], fields.[[Day]], overflow).
   5. Return CreateISODateRecord(referenceISOYear, result.[[Month]], result.[[Day]]).
2. Return ? NonISOMonthDayToISOReferenceDate(calendar, fields, overflow).

The fields of the returned ISO Date Record represent a reference date in the ISO 8601 calendar that, when converted to calendar, corresponds to the month code and day of fields in an arbitrary but deterministically chosen reference year. For the ISO 8601 calendar, the reference year is always 1972. For other calendars, see the note in NonISOMonthDayToISOReferenceDate.

Like RegulateISODate, the operation throws a RangeError exception if overflow is reject and the month and day described by fields does not exist (or does not exist within the year described by fields when there is such a year).

Also like RegulateISODate, if overflow is constrain and the date or month and day described by fields does not exist (or does not exist within the year described by fields when there is such a year), the values of those fields are clamped to their respective valid range. As in CalendarDateToISO, such clamping is calendar-specific.

# 12.3.25 NonISOCalendarISOToDate ( calendar, isoDate )

The implementation-defined abstract operation NonISOCalendarISOToDate takes arguments calendar (a calendar type that is not "iso8601") and isoDate (an ISO Date Record) and returns a Calendar Date Record. It performs implementation-defined processing to find the date corresponding to isoDate in the context of the calendar represented by calendar and returns a Calendar Date Record representing that calendar date, with its fields filled in according to their descriptions in Table 18.

Note

Implementations are encouraged to elide fields that are not read by the caller.

# 12.3.26 CalendarISOToDate ( calendar, isoDate )

The abstract operation CalendarISOToDate takes arguments calendar (a calendar type) and isoDate (an ISO Date Record) and returns a Calendar Date Record. It finds the date corresponding to isoDate in the context of the calendar represented by calendar and returns a Calendar Date Record representing that calendar date, with its fields filled in according to their descriptions. It performs the following steps when called:

1. If calendar is "iso8601", then
   1. If MathematicalInLeapYear(EpochTimeForYear(isoDate.[[Year]])) = 1, let inLeapYear be true; else let inLeapYear be false.
   2. Return Calendar Date Record { [[Era]]: undefined, [[EraYear]]: undefined, [[Year]]: isoDate.[[Year]], [[Month]]: isoDate.[[Month]], [[MonthCode]]: CreateMonthCode(isoDate.[[Month]], false), [[Day]]: isoDate.[[Day]], [[DayOfWeek]]: ISODayOfWeek(isoDate), [[DayOfYear]]: ISODayOfYear(isoDate), [[WeekOfYear]]: ISOWeekOfYear(isoDate), [[DaysInWeek]]: 7, [[DaysInMonth]]: ISODaysInMonth(isoDate.[[Year]], isoDate.[[Month]]), [[DaysInYear]]: MathematicalDaysInYear(isoDate.[[Year]]), [[MonthsInYear]]: 12, [[InLeapYear]]: inLeapYear }.
2. Return NonISOCalendarISOToDate(calendar, isoDate).

Note

Implementations are encouraged to elide fields that are not read by the caller.

# 12.3.27 CalendarExtraFields ( calendar, fields )

The implementation-defined abstract operation CalendarExtraFields takes arguments calendar (a calendar type) and fields (a List of values from the Enumeration Key column of Table 19) and returns a List of values from the Enumeration Key column of Table 19. It characterizes calendar-specific fields that are relevant for the provided fields in the built-in calendar identified by calendar. For example, « era, era-year » is returned when calendar is "gregory" or "japanese" and fields is a List containing year. It performs the following steps when called:

1. If calendar is "iso8601", return a new empty List.
2. Return an implementation-defined List as described above.

# 12.3.28 NonISOFieldKeysToIgnore ( calendar, keys )

The implementation-defined abstract operation NonISOFieldKeysToIgnore takes arguments calendar (a calendar type that is not "iso8601") and keys (a List of values from the Enumeration Key column of Table 19) and returns a List of values from the Enumeration Key column of Table 19. It performs implementation-defined processing to determine which calendar date fields changing any of the fields named in keys can potentially conflict with or invalidate, for the given calendar. A field always invalidates at least itself.

This operation is relevant for calendars which accept fields other than the standard set of ISO 8601 calendar fields, in order to implement the Temporal objects' `with()` methods in such a way that the result is free of ambiguity or conflicts.

For example, given a calendar that uses eras, such as "gregory", a key in keys being any one of year, era, or era-year would exclude all three. Passing any one of the three to a `with()` method might conflict with either of the other two properties on the receiver object, so those properties of the receiver object should be ignored. Given this, in addition to the ISO 8601 mutual exclusion of month and month-code, a possible implementation might produce the following results when calendar is "gregory":

| Table 20: Example results of CalendarFieldKeysToIgnore keys | Returned List                                   |
| ----------------------------------------------------------- | ----------------------------------------------- |
| « era »                                                     | « era, era-year, year »                         |
| « era-year »                                                | « era, era-year, year »                         |
| « year »                                                    | « era, era-year, year »                         |
| « month »                                                   | « month, month-code »                           |
| « month-code »                                              | « month, month-code »                           |
| « day »                                                     | « day »                                         |
| « year, month, day »                                        | « era, era-year, year, month, month-code, day » |
| Note                                                        |                                                 |

In a calendar such as "japanese" where eras do not start and end at year and/or month boundaries, note that the returned List should contain era and era-year if keys contains day, month, or month-code (not only if it contains era, era-year, or year, as in the example above) because it's possible for changing the day or month to cause a conflict with the era.

# 12.3.29 CalendarFieldKeysToIgnore ( calendar, keys )

The abstract operation CalendarFieldKeysToIgnore takes arguments calendar (a calendar type) and keys (a List of values from the Enumeration Key column of Table 19) and returns a List of values from the Enumeration Key column of Table 19. It determines which calendar date fields changing any of the fields named in keys can potentially conflict with or invalidate, for the given calendar. A field always invalidates at least itself. It performs the following steps when called:

1. If calendar is "iso8601", then
   1. Let ignoredKeys be a new empty List.
   2. For each element key of keys, do
      1. Append key to ignoredKeys.
      2. If key is month, append month-code to ignoredKeys.
      3. Else if key is month-code, append month to ignoredKeys.
   3. NOTE: While ignoredKeys can have duplicate elements, this is not intended to be meaningful. This specification only checks whether particular keys are or are not members of the list.
   4. Return ignoredKeys.
2. Return NonISOFieldKeysToIgnore(calendar, keys).

# 12.3.30 NonISOResolveFields ( calendar, fields, type )

The implementation-defined abstract operation NonISOResolveFields takes arguments calendar (a calendar type that is not "iso8601"), fields (a Calendar Fields Record), and type (one of date, year-month, or month-day) and returns either a normal completion containing unused or a throw completion. It performs implementation-defined processing to validate that fields (which describes a date or partial date in the built-in calendar identified by calendar) is sufficiently complete to satisfy type and not internally inconsistent, and mutates fields into acceptable input for CalendarDateToISO ( calendar, fields, overflow ) or CalendarMonthDayToISOReferenceDate ( calendar, fields, overflow ) by merging data that can be represented in multiple forms into standard fields and removing redundant fields (for example, merging [[Era]] and [[EraYear]] into [[Year]]).

The operation throws a TypeError exception if the non-unset fields of fields are insufficient to identify a unique instance of type in the calendar (e.g., when at least one field in each combination capable of determining some part of its data is unset) or a RangeError exception if the fields are sufficient but their values are internally inconsistent within the calendar (e.g., when fields such as [[Month]] and [[MonthCode]] have conflicting non-unset values). For example:

- If type is either date or month-day, and "day" in the calendar has an interpretation similar to ISO 8601 and fields.[[Day]] is unset.
- If fields.[[MonthCode]] identifies a month code that is not valid in any year of the calendar.
- If fields.[[Month]] and fields.[[MonthCode]] are both unset or neither value is unset but they do not identify the same month.
- If type is month-day and fields.[[MonthCode]] is unset and a specific year cannot be determined from fields.
- If the calendar supports the usual partitioning of years into eras with their own year counting as represented by "year", "era", and "era year" (as in the Gregorian or traditional Japanese calendars) and any of the following cases apply:
  - type is date or year-month and each of fields.[[Year]], fields.[[Era]], and fields.[[EraYear]] is unset.
  - fields.[[Era]] is unset but fields.[[EraYear]] is not.
  - fields.[[EraYear]] is unset but fields.[[Era]] is not.
  - None of the three values are unset but fields.[[Era]] and fields.[[EraYear]] do not together identify the same year as fields.[[Year]].

Note 1

In some cases, verifying the internal consistency of two fields requires the data from other fields, such as checking fields.[[MonthCode]] "M06" against fields.[[Month]] 7 in the Hebrew calendar (which are consistent if and only if fields identifies a year that includes leap month Adar I).

Note 2

When the fields of fields are inconsistent with respect to a non-unset fields.[[Era]], it is recommended that fields.[[Era]] and fields.[[EraYear]] be updated to resolve the inconsistency by lenient interpretation of out-of-bounds values (rather than throwing a RangeError), which is particularly useful for consistent interpretation of dates in calendars with regnal eras.

- In the Gregorian calendar, a zero or negative fields.[[EraYear]] should be replaced with a positive [[EraYear]] corresponding with extension of the era into its complement and fields.[[Era]] should be updating accordingly (such that Common Era [[EraYear]] 0 is updated to Before Common Era [[EraYear]] 1, Before Common Era [[EraYear]] -1 is updated to Common Era [[EraYear]] 2, etc.).
- In the Japanese calendar, when fields.[[Era]] is not unset and the date represented by fields is not within the bounds of that era, fields.[[Era]] should be updated to the appropriate containing era for that date (for example, because the transition from Heisei era [[EraYear]] 31 to Reiwa era [[EraYear]] 1 took place on May 1 of [[Year]] 2019, Heisei era [[EraYear]] 32 should be updated to Reiwa era [[EraYear]] 2, Reiwa era [[EraYear]] 1 [[Month]] 1 should be updated to Heisei era [[EraYear]] 31 [[Month]] 1, etc.).

Note 3

When type is month-day and fields.[[Month]] is not unset, it is recommended that all built-in calendars other than the ISO 8601 calendar require a disambiguating year (e.g., either fields.[[Year]] or fields.[[Era]] and fields.[[EraYear]]) to avoid a TypeError, regardless of whether or not fields.[[MonthCode]] is also unset. The ISO 8601 calendar allows fields.[[Year]] to be unset in this case because it is a special default calendar that is permanently stable for automated processing.

# 12.3.31 CalendarResolveFields ( calendar, fields, type )

The abstract operation CalendarResolveFields takes arguments calendar (a calendar type), fields (a Calendar Fields Record), and type (one of date, year-month, or month-day) and returns either a normal completion containing unused or a throw completion. It validates that fields (which describes a date or partial date in the built-in calendar identified by calendar) is sufficiently complete to satisfy type and not internally inconsistent, and mutates fields into acceptable input for CalendarDateToISO ( calendar, fields, overflow ) or CalendarMonthDayToISOReferenceDate ( calendar, fields, overflow ) by merging data that can be represented in multiple forms into standard fields and removing redundant fields (for example, merging [[Era]] and [[EraYear]] into [[Year]]). It performs the following steps when called:

1. If calendar is "iso8601", then
   1. Let needsYear be false.
   2. If type is either date or year-month, set needsYear to true.
   3. Let needsDay be false.
   4. If type is either date or month-day, set needsDay to true.
   5. If needsYear is true and fields.[[Year]] is unset, throw a TypeError exception.
   6. If needsDay is true and fields.[[Day]] is unset, throw a TypeError exception.
   7. If fields.[[Month]] is unset and fields.[[MonthCode]] is unset, throw a TypeError exception.
   8. If fields.[[MonthCode]] is not unset, then
      1. Let parsedMonthCode be ! ParseMonthCode(fields.[[MonthCode]]).
      2. If parsedMonthCode.[[IsLeapMonth]] is true, throw a RangeError exception.
      3. Let month be parsedMonthCode.[[MonthNumber]].
      4. If month > 12, throw a RangeError exception.
      5. If fields.[[Month]] is not unset and fields.[[Month]] ≠ month, throw a RangeError exception.
      6. Set fields.[[Month]] to month.
2. Else,
   1. Perform ? NonISOResolveFields(calendar, fields, type).
3. Return unused.

# 13 Abstract Operations

# 13.1 ISODateToEpochDays ( year, month, date )

The abstract operation ISODateToEpochDays takes arguments year (an integer), month (an integer), and date (an integer) and returns an integer. It calculates a number of days. It performs the following steps when called:

1. Let resolvedYear be year \+ floor(month / 12).
2. Let resolvedMonth be month modulo 12.
3. Find a time t such that EpochTimeToEpochYear(t) = resolvedYear, EpochTimeToMonthInYear(t) = resolvedMonth, and EpochTimeToDate(t) = 1.
4. Return EpochTimeToDayNumber(t) + date \- 1.

Editor's Note

This operation corresponds to ECMA-262 operation MakeDay(year, month, date). It calculates the result in mathematical values instead of Numbers. These two operations would be unified when https://github.com/tc39/ecma262/issues/1087 is fixed.

# 13.2 EpochDaysToEpochMs ( day, time )

The abstract operation EpochDaysToEpochMs takes arguments day (an integer) and time (an integer) and returns an integer. It calculates a number of milliseconds. It performs the following steps when called:

1. Return day × ℝ(msPerDay) + time.

Editor's Note

This operation corresponds to ECMA-262 operation MakeDate(date, time). It calculates the result in mathematical values instead of Numbers. These two operations would be unified when https://github.com/tc39/ecma262/issues/1087 is fixed.

# 13.3 Date Equations

A given time t belongs to day number

EpochTimeToDayNumber(t) = floor(t / ℝ(msPerDay))

Number of days in year are given by:

MathematicalDaysInYear(y)

= 365 if ((y) modulo 4) ≠ 0

= 366 if ((y) modulo 4) = 0 and ((y) modulo 100) ≠ 0

= 365 if ((y) modulo 100) = 0 and ((y) modulo 400) ≠ 0

= 366 if ((y) modulo 400) = 0

The day number of the first day of year y is given by:

EpochDayNumberForYear(y) = 365 × (y \- 1970) + floor((y \- 1969) / 4) - floor((y \- 1901) / 100) + floor((y \- 1601) / 400)

The time of the start of a year is:

EpochTimeForYear(y) = ℝ(msPerDay) × EpochDayNumberForYear(y)

Epoch year from time t is given by:

EpochTimeToEpochYear(t) = the largest integral Number y (closest to +∞) such that EpochTimeForYear(y) ≤ t

The following function returns 1 for a time within leap year otherwise it returns 0:

MathematicalInLeapYear(t)

= 0 if MathematicalDaysInYear(EpochTimeToEpochYear(t)) = 365

= 1 if MathematicalDaysInYear(EpochTimeToEpochYear(t)) = 366

The month number for a time t is given by:

EpochTimeToMonthInYear(t)

= 0 if 0 ≤ EpochTimeToDayInYear(t) < 31

= 1 if 31 ≤ EpochTimeToDayInYear(t) < 59 + MathematicalInLeapYear(t)

= 2 if 59 + MathematicalInLeapYear(t) ≤ EpochTimeToDayInYear(t) < 90 + MathematicalInLeapYear(t)

= 3 if 90 + MathematicalInLeapYear(t) ≤ EpochTimeToDayInYear(t) < 120 + MathematicalInLeapYear(t)

= 4 if 120 + MathematicalInLeapYear(t) ≤ EpochTimeToDayInYear(t) < 151 + MathematicalInLeapYear(t)

= 5 if 151 + MathematicalInLeapYear(t) ≤ EpochTimeToDayInYear(t) < 181 + MathematicalInLeapYear(t)

= 6 if 181 + MathematicalInLeapYear(t) ≤ EpochTimeToDayInYear(t) < 212 + MathematicalInLeapYear(t)

= 7 if 212 + MathematicalInLeapYear(t) ≤ EpochTimeToDayInYear(t) < 243 + MathematicalInLeapYear(t)

= 8 if 243 + MathematicalInLeapYear(t) ≤ EpochTimeToDayInYear(t) < 273 + MathematicalInLeapYear(t)

= 9 if 273 + MathematicalInLeapYear(t) ≤ EpochTimeToDayInYear(t) < 304 + MathematicalInLeapYear(t)

= 10 if 304 + MathematicalInLeapYear(t) ≤ EpochTimeToDayInYear(t) < 334 + MathematicalInLeapYear(t)

= 11 if 334 + MathematicalInLeapYear(t) ≤ EpochTimeToDayInYear(t) < 365 + MathematicalInLeapYear(t)

where

EpochTimeToDayInYear(t) = EpochTimeToDayNumber(t) - EpochDayNumberForYear(EpochTimeToEpochYear(t))

A month value of 0 specifies January; 1 specifies February; 2 specifies March; 3 specifies April; 4 specifies May; 5 specifies June; 6 specifies July; 7 specifies August; 8 specifies September; 9 specifies October; 10 specifies November; and 11 specifies December. Note that EpochTimeToMonthInYear(0) = 0, corresponding to Thursday, 1 January 1970.

The date number for a time t is given by:

EpochTimeToDate(t)

= EpochTimeToDayInYear(t) + 1 if EpochTimeToMonthInYear(t) = 0

= EpochTimeToDayInYear(t) - 30 if EpochTimeToMonthInYear(t) = 1

= EpochTimeToDayInYear(t) - 58 - MathematicalInLeapYear(t) if EpochTimeToMonthInYear(t) = 2

= EpochTimeToDayInYear(t) - 89 - MathematicalInLeapYear(t) if EpochTimeToMonthInYear(t) = 3

= EpochTimeToDayInYear(t) - 119 - MathematicalInLeapYear(t) if EpochTimeToMonthInYear(t) = 4

= EpochTimeToDayInYear(t) - 150 - MathematicalInLeapYear(t) if EpochTimeToMonthInYear(t) = 5

= EpochTimeToDayInYear(t) - 180 - MathematicalInLeapYear(t) if EpochTimeToMonthInYear(t) = 6

= EpochTimeToDayInYear(t) - 211 - MathematicalInLeapYear(t) if EpochTimeToMonthInYear(t) = 7

= EpochTimeToDayInYear(t) - 242 - MathematicalInLeapYear(t) if EpochTimeToMonthInYear(t) = 8

= EpochTimeToDayInYear(t) - 272 - MathematicalInLeapYear(t) if EpochTimeToMonthInYear(t) = 9

= EpochTimeToDayInYear(t) - 303 - MathematicalInLeapYear(t) if EpochTimeToMonthInYear(t) = 10

= EpochTimeToDayInYear(t) - 333 - MathematicalInLeapYear(t) if EpochTimeToMonthInYear(t) = 11

The weekday for a particular time t is defined as:

EpochTimeToWeekDay(t) =(EpochTimeToDayNumber(t) + 4) modulo 7

A weekday value of 0 specifies Sunday; 1 specifies Monday; 2 specifies Tuesday; 3 specifies Wednesday; 4 specifies Thursday; 5 specifies Friday; and 6 specifies Saturday. Note that EpochTimeToWeekDay(0) = 4, corresponding to Thursday, 1 January 1970.

Editor's Note

These equations correspond to ECMA-262 equations defined in Days in Year, Month from Time, Date from Time, Week Day respectively. These calculate the result in mathematical values instead of Numbers. These equations would be unified when https://github.com/tc39/ecma262/issues/1087 is fixed.

Editor's Note

Note that the operation EpochTimeToMonthInYear(t) uses 0-based months unlike rest of Temporal since it's intended to be unified with MonthFromTime(t) when the above mentioned issue is fixed.

# 13.4 CheckISODaysRange ( isoDate )

The abstract operation CheckISODaysRange takes argument isoDate (an ISO Date Record) and returns either a normal completion containing unused or a throw completion. It checks that the given date is within the range of 108 days from the epoch. It performs the following steps when called:

1. If abs(ISODateToEpochDays(isoDate.[[Year]], isoDate.[[Month]] \- 1, isoDate.[[Day]])) > 108, throw a RangeError exception.
2. Return unused.

Editor's Note

This operation is solely present to ensure that GetUTCEpochNanoseconds is not called with numbers that are too large. It is distinct from ISODateWithinLimits, which uses GetUTCEpochNanoseconds. This operation can be removed with no observable effect when https://github.com/tc39/ecma262/issues/1087 is fixed.

# 13.5 Units

Time is reckoned using multiple units. These units are listed in Table 21.

A Temporal unit is a value listed in the "Value" column of Table 21. A calendar unit is a Temporal unit for which IsCalendarUnit returns true. A date unit is a Temporal unit for which the corresponding "Category" value in Table 21 is date, and a time unit is a Temporal unit for which the corresponding "Category" value is time.

| Table 21: Temporal units by descending magnitude Value | Singular property name | Plural property name | Category | Length in nanoseconds | Maximum duration rounding increment |
| ------------------------------------------------------ | ---------------------- | -------------------- | -------- | --------------------- | ----------------------------------- |
| year                                                   | "year"                 | "years"              | date     | calendar-dependent    | unset                               |
| month                                                  | "month"                | "months"             | date     | calendar-dependent    | unset                               |
| week                                                   | "week"                 | "weeks"              | date     | calendar-dependent    | unset                               |
| day                                                    | "day"                  | "days"               | date     | nsPerDay              | unset                               |
| hour                                                   | "hour"                 | "hours"              | time     | 3.6 × 1012            | 24                                  |
| minute                                                 | "minute"               | "minutes"            | time     | 6 × 1010              | 60                                  |
| second                                                 | "second"               | "seconds"            | time     | 109                   | 60                                  |
| millisecond                                            | "millisecond"          | "milliseconds"       | time     | 106                   | 1000                                |
| microsecond                                            | "microsecond"          | "microseconds"       | time     | 103                   | 1000                                |
| nanosecond                                             | "nanosecond"           | "nanoseconds"        | time     | 1                     | 1000                                |
| Note                                                   |                        |                      |          |                       |                                     |

The length of a day is given as nsPerDay in the table. Note that changes in the UTC offset of a time zone may result in longer or shorter days, so care should be taken when using this value in the context of `Temporal.ZonedDateTime`.

# 13.6 GetTemporalOverflowOption ( options )

The abstract operation GetTemporalOverflowOption takes argument options (an Object) and returns either a normal completion containing either constrain or reject, or a throw completion. It fetches and validates the "overflow" property of options, returning a default if absent. It performs the following steps when called:

1. Let stringValue be ? GetOption(options, "overflow", string, « "constrain", "reject" », "constrain").
2. If stringValue is "constrain", return constrain.
3. Return reject.

# 13.7 GetTemporalDisambiguationOption ( options )

The abstract operation GetTemporalDisambiguationOption takes argument options (an Object) and returns either a normal completion containing either compatible, earlier, later, or reject, or a throw completion. It fetches and validates the "disambiguation" property of options, returning a default if absent. It performs the following steps when called:

1. Let stringValue be ? GetOption(options, "disambiguation", string, « "compatible", "earlier", "later", "reject" », "compatible").
2. If stringValue is "compatible", return compatible.
3. If stringValue is "earlier", return earlier.
4. If stringValue is "later", return later.
5. Return reject.

# 13.8 NegateRoundingMode ( roundingMode )

The abstract operation NegateRoundingMode takes argument roundingMode (a rounding mode) and returns a rounding mode. It returns the correct rounding mode to use when rounding the negative of a value that was originally given with roundingMode. It performs the following steps when called:

1. If roundingMode is ceil, return floor.
2. If roundingMode is floor, return ceil.
3. If roundingMode is half-ceil, return half-floor.
4. If roundingMode is half-floor, return half-ceil.
5. Return roundingMode.

# 13.9 GetTemporalOffsetOption ( options, fallback )

The abstract operation GetTemporalOffsetOption takes arguments options (an Object) and fallback (one of prefer, use, ignore, or reject) and returns either a normal completion containing either prefer, use, ignore, or reject, or a throw completion. It fetches and validates the "offset" property of options, returning fallback as a default if absent. It performs the following steps when called:

1. If fallback is prefer, let stringFallback be "prefer".
2. Else if fallback is use, let stringFallback be "use".
3. Else if fallback is ignore, let stringFallback be "ignore".
4. Else, let stringFallback be "reject".
5. Let stringValue be ? GetOption(options, "offset", string, « "prefer", "use", "ignore", "reject" », stringFallback).
6. If stringValue is "prefer", return prefer.
7. If stringValue is "use", return use.
8. If stringValue is "ignore", return ignore.
9. Return reject.

# 13.10 GetTemporalShowCalendarNameOption ( options )

The abstract operation GetTemporalShowCalendarNameOption takes argument options (an Object) and returns either a normal completion containing either auto, always, never, or critical, or a throw completion. It fetches and validates the "calendarName" property from options, returning a default if absent.

Note

This property is used in `toString` methods in Temporal to control whether a calendar annotation should be output.

It performs the following steps when called:

1. Let stringValue be ? GetOption(options, "calendarName", string, « "auto", "always", "never", "critical" », "auto").
2. If stringValue is "always", return always.
3. If stringValue is "never", return never.
4. If stringValue is "critical", return critical.
5. Return auto.

# 13.11 GetTemporalShowTimeZoneNameOption ( options )

The abstract operation GetTemporalShowTimeZoneNameOption takes argument options (an Object) and returns either a normal completion containing either auto, never, or critical, or a throw completion. It fetches and validates the "timeZoneName" property from options, returning a default if absent.

Note

This property is used in `Temporal.ZonedDateTime.prototype.toString()`. It is different from the `timeZone` property passed to `Temporal.ZonedDateTime.from()` and from the `timeZone` property in the options passed to `Temporal.Instant.prototype.toString()`.

It performs the following steps when called:

1. Let stringValue be ? GetOption(options, "timeZoneName", string, « "auto", "never", "critical" », "auto").
2. If stringValue is "never", return never.
3. If stringValue is "critical", return critical.
4. Return auto.

# 13.12 GetTemporalShowOffsetOption ( options )

The abstract operation GetTemporalShowOffsetOption takes argument options (an Object) and returns either a normal completion containing either auto or never, or a throw completion. It fetches and validates the "offset" property from options, returning a default if absent.

Note

This property is used in `Temporal.ZonedDateTime.prototype.toString()`. It is different from the `offset` property passed to `Temporal.ZonedDateTime.from()`.

It performs the following steps when called:

1. Let stringValue be ? GetOption(options, "offset", string, « "auto", "never" », "auto").
2. If stringValue is "never", return never.
3. Return auto.

# 13.13 GetDirectionOption ( options )

The abstract operation GetDirectionOption takes argument options (an Object) and returns either a normal completion containing either next or previous, or a throw completion. It fetches and validates the "direction" property from options, throwing if absent. It performs the following steps when called:

1. Let stringValue be ? GetOption(options, "direction", string, « "next", "previous" », required).
2. If stringValue is "next", return next.
3. Return previous.

# 13.14 ValidateTemporalRoundingIncrement ( increment, dividend, inclusive )

The abstract operation ValidateTemporalRoundingIncrement takes arguments increment (a positive integer), dividend (a positive integer), and inclusive (a Boolean) and returns either a normal completion containing unused or a throw completion. It verifies that increment evenly divides dividend, otherwise throwing a RangeError. dividend must be divided into more than one part unless inclusive is true. It performs the following steps when called:

1. If inclusive is true, then
   1. Let maximum be dividend.
2. Else,
   1. Assert: dividend > 1.
   2. Let maximum be dividend \- 1.
3. If increment > maximum, throw a RangeError exception.
4. If dividend modulo increment ≠ 0, throw a RangeError exception.
5. Return unused.

# 13.15 GetTemporalFractionalSecondDigitsOption ( options )

The abstract operation GetTemporalFractionalSecondDigitsOption takes argument options (an Object) and returns either a normal completion containing either auto or an integer in the inclusive interval from 0 to 9, or a throw completion. It fetches and validates the "fractionalSecondDigits" property from options, returning a default if absent. It performs the following steps when called:

1. Let digitsValue be ? Get(options, "fractionalSecondDigits").
2. If digitsValue is undefined, return auto.
3. If digitsValue is not a Number, then
   1. If ? ToString(digitsValue) is not "auto", throw a RangeError exception.
   2. Return auto.
4. If digitsValue is one of NaN, +∞𝔽, or -∞𝔽, throw a RangeError exception.
5. Let digitCount be floor(ℝ(digitsValue)).
6. If digitCount < 0 or digitCount > 9, throw a RangeError exception.
7. Return digitCount.

# 13.16 ToSecondsStringPrecisionRecord ( smallestUnit, fractionalDigitCount )

The abstract operation ToSecondsStringPrecisionRecord takes arguments smallestUnit (one of minute, second, millisecond, microsecond, nanosecond, or unset) and fractionalDigitCount (either an integer in the inclusive interval from 0 to 9 or auto) and returns a Record with fields [[Precision]] (either an integer in the inclusive interval from 0 to 9, minute, or auto), [[Unit]] (one of minute, second, millisecond, microsecond, or nanosecond), and [[Increment]] (one of 1, 10, or 100). The returned Record represents details for serializing minutes and seconds to a string subject to the specified smallestUnit or (when smallestUnit is unset) fractionalDigitCount digits after the decimal point in the seconds. Its [[Precision]] field is either that count of digits, the value auto signifying that there should be no insignificant trailing zeroes, or the value minute signifying that seconds should not be included at all. Its [[Unit]] field is the most precise unit that can contribute to the string, and its [[Increment]] field indicates the rounding increment that should be applied to that unit. It performs the following steps when called:

1. If smallestUnit is minute, return the Record { [[Precision]]: minute, [[Unit]]: minute, [[Increment]]: 1 }.
2. If smallestUnit is second, return the Record { [[Precision]]: 0, [[Unit]]: second, [[Increment]]: 1 }.
3. If smallestUnit is millisecond, return the Record { [[Precision]]: 3, [[Unit]]: millisecond, [[Increment]]: 1 }.
4. If smallestUnit is microsecond, return the Record { [[Precision]]: 6, [[Unit]]: microsecond, [[Increment]]: 1 }.
5. If smallestUnit is nanosecond, return the Record { [[Precision]]: 9, [[Unit]]: nanosecond, [[Increment]]: 1 }.
6. Assert: smallestUnit is unset.
7. If fractionalDigitCount is auto, return the Record { [[Precision]]: auto, [[Unit]]: nanosecond, [[Increment]]: 1 }.
8. If fractionalDigitCount = 0, return the Record { [[Precision]]: 0, [[Unit]]: second, [[Increment]]: 1 }.
9. If fractionalDigitCount is in the inclusive interval from 1 to 3, return the Record { [[Precision]]: fractionalDigitCount, [[Unit]]: millisecond, [[Increment]]: 103 - fractionalDigitCount }.
10. If fractionalDigitCount is in the inclusive interval from 4 to 6, return the Record { [[Precision]]: fractionalDigitCount, [[Unit]]: microsecond, [[Increment]]: 106 - fractionalDigitCount }.
11. Assert: fractionalDigitCount is in the inclusive interval from 7 to 9.
12. Return the Record { [[Precision]]: fractionalDigitCount, [[Unit]]: nanosecond, [[Increment]]: 109 - fractionalDigitCount }.

# 13.17 GetTemporalUnitValuedOption ( options, key, default )

The abstract operation GetTemporalUnitValuedOption takes arguments options (an Object), key (a property key), and default (either required or unset) and returns either a normal completion containing either a Temporal unit, unset, or auto, or a throw completion. It attempts to read a Temporal unit from the specified property of options.

Both singular and plural unit names are accepted, but only the singular form is used internally.

1. Let allowedStrings be a List containing all values in the "Singular property name" and "Plural property name" columns of Table 21, except the header row.
2. Append "auto" to allowedStrings.
3. NOTE: For each singular Temporal unit name that is contained within allowedStrings, the corresponding plural name is also contained within it.
4. If default is unset, then
   1. Let defaultValue be undefined.
5. Else,
   1. Let defaultValue be default.
6. Let value be ? GetOption(options, key, string, allowedStrings, defaultValue).
7. If value is undefined, return unset.
8. If value is "auto", return auto.
9. Return the value in the "Value" column of Table 21 corresponding to the row with value in its "Singular property name" or "Plural property name" column.

# 13.18 ValidateTemporalUnitValue ( value, unitGroup [ , extraValues ] )

The abstract operation ValidateTemporalUnitValue takes arguments value (either a Temporal unit, unset, or auto) and unitGroup (one of date, time, or datetime) and optional argument extraValues (a List of either Temporal units or auto) and returns either a normal completion containing unused or a throw completion. It validates that the result of GetTemporalUnitValuedOption is covered by the union of unitGroup, extraValues, and « unset ». It performs the following steps when called:

1. If value is unset, return unused.
2. If extraValues is present and extraValues contains value, return unused.
3. Let category be the value in the “Category” column of the row of Table 21 whose “Value” column contains value. If there is no such row, throw a RangeError exception.
4. If category is date and unitGroup is either date or datetime, return unused.
5. If category is time and unitGroup is either time or datetime, return unused.
6. Throw a RangeError exception.

# 13.19 GetTemporalRelativeToOption ( options )

The abstract operation GetTemporalRelativeToOption takes argument options (an Object) and returns either a normal completion containing a Record with fields [[PlainRelativeTo]] (a Temporal.PlainDate or undefined) and [[ZonedRelativeTo]] (a Temporal.ZonedDateTime or undefined), or a throw completion. It examines the value of the `relativeTo` property of its options argument. If the value is undefined, both the [[PlainRelativeTo]] and [[ZonedRelativeTo]] fields of the returned Record are undefined. If the value is not a String or an Object, it throws a TypeError. Otherwise, it attempts to return a Temporal.ZonedDateTime instance in the [[ZonedRelativeTo]] field, or a Temporal.PlainDate instance in the [[PlainRelativeTo]] field, in order of preference, by converting the value. If neither of those are possible, it throws a RangeError. It performs the following steps when called:

1. Let value be ? Get(options, "relativeTo").
2. If value is undefined, return the Record { [[PlainRelativeTo]]: undefined, [[ZonedRelativeTo]]: undefined }.
3. Let offsetBehaviour be option.
4. Let matchBehaviour be match-exactly.
5. If value is an Object, then
   1. If value has an [[InitializedTemporalZonedDateTime]] internal slot, then
      1. Return the Record { [[PlainRelativeTo]]: undefined, [[ZonedRelativeTo]]: value }.
   2. If value has an [[InitializedTemporalDate]] internal slot, then
      1. Return the Record { [[PlainRelativeTo]]: value, [[ZonedRelativeTo]]: undefined }.
   3. If value has an [[InitializedTemporalDateTime]] internal slot, then
      1. Let plainDate be ! CreateTemporalDate(value.[[ISODateTime]].[[ISODate]], value.[[Calendar]]).
      2. Return the Record { [[PlainRelativeTo]]: plainDate, [[ZonedRelativeTo]]: undefined }.
   4. Let calendar be ? GetTemporalCalendarIdentifierWithISODefault(value).
   5. Let fields be ? PrepareCalendarFields(calendar, value, « year, month, month-code, day », « hour, minute, second, millisecond, microsecond, nanosecond, offset, time-zone », «»).
   6. Let result be ? InterpretTemporalDateTimeFields(calendar, fields, constrain).
   7. Let timeZone be fields.[[TimeZone]].
   8. Let offsetString be fields.[[OffsetString]].
   9. If offsetString is unset, then
      1. Set offsetBehaviour to wall.
   10. Let isoDate be result.[[ISODate]].
   11. Let time be result.[[Time]].
6. Else,
   1. If value is not a String, throw a TypeError exception.
   2. Let result be ? ParseISODateTime(value, « TemporalDateTimeString[+Zoned], TemporalDateTimeString[~Zoned] »).
   3. Let offsetString be result.[[TimeZone]].[[OffsetString]].
   4. Let annotation be result.[[TimeZone]].[[TimeZoneAnnotation]].
   5. If annotation is empty, then
      1. Let timeZone be unset.
   6. Else,
      1. Let timeZone be ? ToTemporalTimeZoneIdentifier(annotation).
      2. If result.[[TimeZone]].[[Z]] is true, then
         1. Set offsetBehaviour to exact.
      3. Else if offsetString is empty, then
         1. Set offsetBehaviour to wall.
      4. Set matchBehaviour to match-minutes.
      5. If offsetString is not empty, then
         1. Let offsetParseResult be ParseText(StringToCodePoints(offsetString), UTCOffset[+SubMinutePrecision]).
         2. Assert: offsetParseResult is a Parse Node.
         3. If offsetParseResult contains more than one MinuteSecond Parse Node, set matchBehaviour to match-exactly.
   7. Let calendar be result.[[Calendar]].
   8. If calendar is empty, set calendar to "iso8601".
   9. Set calendar to ? CanonicalizeCalendar(calendar).
   10. Let isoDate be CreateISODateRecord(result.[[Year]], result.[[Month]], result.[[Day]]).
   11. Let time be result.[[Time]].
7. If timeZone is unset, then
   1. Let plainDate be ? CreateTemporalDate(isoDate, calendar).
   2. Return the Record { [[PlainRelativeTo]]: plainDate, [[ZonedRelativeTo]]: undefined }.
8. If offsetBehaviour is option, then
   1. Let offsetNs be ! ParseDateTimeUTCOffset(offsetString).
9. Else,
   1. Let offsetNs be 0.
10. Let epochNanoseconds be ? InterpretISODateTimeOffset(isoDate, time, offsetBehaviour, offsetNs, timeZone, compatible, reject, matchBehaviour).
11. Let zonedRelativeTo be ! CreateTemporalZonedDateTime(epochNanoseconds, timeZone, calendar).
12. Return the Record { [[PlainRelativeTo]]: undefined, [[ZonedRelativeTo]]: zonedRelativeTo }.

# 13.20 LargerOfTwoTemporalUnits ( u1, u2 )

The abstract operation LargerOfTwoTemporalUnits takes arguments u1 (a Temporal unit) and u2 (a Temporal unit) and returns a Temporal unit. Given two Temporal units, it returns the larger of the two units. It performs the following steps when called:

1. For each row of Table 21, except the header row, in table order, do
   1. Let unit be the value in the "Value" column of the row.
   2. If u1 is unit, return unit.
   3. If u2 is unit, return unit.

# 13.21 IsCalendarUnit ( unit )

The abstract operation IsCalendarUnit takes argument unit (a Temporal unit) and returns a Boolean. It returns whether unit is a Temporal unit for which rounding would require calendar calculations. It performs the following steps when called:

1. If unit is year, return true.
2. If unit is month, return true.
3. If unit is week, return true.
4. Return false.

# 13.22 TemporalUnitCategory ( unit )

The abstract operation TemporalUnitCategory takes argument unit (a Temporal unit) and returns either date or time. It returns the category (date or time) of the Temporal unit unit. It performs the following steps when called:

1. Return the value from the "Category" column of the row of Table 21 in which unit is in the "Value" column.

# 13.23 MaximumTemporalDurationRoundingIncrement ( unit )

The abstract operation MaximumTemporalDurationRoundingIncrement takes argument unit (a Temporal unit) and returns one of 24, 60, 1000, or unset. Given a string representing a Temporal.Duration unit, it returns the maximum rounding increment for that unit, or unset if there is no maximum. It performs the following steps when called:

1. Return the value from the "Maximum duration rounding increment" column of the row of Table 21 in which unit is in the "Value" column.

# 13.24 IsPartialTemporalObject ( value )

The abstract operation IsPartialTemporalObject takes argument value (an ECMAScript language value) and returns either a normal completion containing a Boolean or a throw completion. It determines whether value is a suitable input for one of the Temporal objects' `with()` methods: it must be an Object, it must not be an instance of one of the time-related or date-related Temporal types, and it must not have a `calendar` or `timeZone` property. It performs the following steps when called:

1. If value is not an Object, return false.
2. If value has an [[InitializedTemporalDate]], [[InitializedTemporalDateTime]], [[InitializedTemporalMonthDay]], [[InitializedTemporalTime]], [[InitializedTemporalYearMonth]], or [[InitializedTemporalZonedDateTime]] internal slot, return false.
3. Let calendarProperty be ? Get(value, "calendar").
4. If calendarProperty is not undefined, return false.
5. Let timeZoneProperty be ? Get(value, "timeZone").
6. If timeZoneProperty is not undefined, return false.
7. Return true.

# 13.25 FormatFractionalSeconds ( subSecondNanoseconds, precision )

The abstract operation FormatFractionalSeconds takes arguments subSecondNanoseconds (an integer in the inclusive interval from 0 to 999999999) and precision (either an integer in the inclusive interval from 0 to 9 or auto) and returns a String. If precision = 0, or precision is auto and subSecondNanoseconds = 0, then an empty String will be returned. Otherwise, the output will be a decimal point followed by a sequence of fractional seconds digits, truncated to precision digits or (if precision is auto) to the last non-zero digit. It performs the following steps when called:

1. If precision is auto, then
   1. If subSecondNanoseconds = 0, return the empty String.
   2. Let fractionString be ToZeroPaddedDecimalString(subSecondNanoseconds, 9).
   3. Set fractionString to the longest prefix of fractionString ending with a code unit other than 0x0030 (DIGIT ZERO).
2. Else,
   1. If precision = 0, return the empty String.
   2. Let fractionString be ToZeroPaddedDecimalString(subSecondNanoseconds, 9).
   3. Set fractionString to the substring of fractionString from 0 to precision.
3. Return the string-concatenation of the code unit 0x002E (FULL STOP) and fractionString.

# 13.26 FormatTimeString ( hour, minute, second, subSecondNanoseconds, precision [ , style ] )

The abstract operation FormatTimeString takes arguments hour (an integer in the inclusive interval from 0 to 23), minute (an integer in the inclusive interval from 0 to 59), second (an integer in the inclusive interval from 0 to 59), subSecondNanoseconds (an integer in the inclusive interval from 0 to 999999999), and precision (either an integer in the inclusive interval from 0 to 9, minute, or auto) and optional argument style (either separated or unseparated) and returns a String. It formats a collection of unsigned time components into a string, truncating units as necessary, and separating hours, minutes, and seconds with colons unless style is unseparated. The output will be formatted like HH:MM or HHMM if precision is minute. Otherwise, the output will be formatted like HH:MM:SS or HHMMSS if precision = 0, or subSecondNanoseconds = 0 and precision is auto. Otherwise, the output will be formatted like HH:MM:SS.fff or HHMMSS.fff where "fff" is a sequence of fractional seconds digits, truncated to precision digits or (if precision is auto) to the last non-zero digit. It performs the following steps when called:

1. If style is present and style is unseparated, let separator be the empty String; else let separator be ":".
2. Let hh be ToZeroPaddedDecimalString(hour, 2).
3. Let mm be ToZeroPaddedDecimalString(minute, 2).
4. If precision is minute, return the string-concatenation of hh, separator, and mm.
5. Let ss be ToZeroPaddedDecimalString(second, 2).
6. Let subSecondsPart be FormatFractionalSeconds(subSecondNanoseconds, precision).
7. Return the string-concatenation of hh, separator, mm, separator, ss, and subSecondsPart.

# 13.27 GetUnsignedRoundingMode ( roundingMode, sign )

The abstract operation GetUnsignedRoundingMode takes arguments roundingMode (a rounding mode) and sign (either negative or positive) and returns an unsigned rounding mode. It returns the rounding mode that should be applied to the absolute value of a number to produce the same result as if roundingMode were applied to the signed value of the number (negative if sign is negative, or positive otherwise). It performs the following steps when called:

1. Return the specification type in the "Unsigned Rounding Mode" column of Table 22 for the row where the value in the "Rounding Mode" column is roundingMode and the value in the "Sign" column is sign.

| Table 22: Conversion from rounding mode to unsigned rounding mode Rounding Mode | Sign          | Unsigned Rounding Mode |
| ------------------------------------------------------------------------------- | ------------- | ---------------------- |
| ceil                                                                            | positive      | infinity               |
| negative                                                                        | zero          |                        |
| floor                                                                           | positive      | zero                   |
| negative                                                                        | infinity      |                        |
| expand                                                                          | positive      | infinity               |
| negative                                                                        | infinity      |                        |
| trunc                                                                           | positive      | zero                   |
| negative                                                                        | zero          |                        |
| half-ceil                                                                       | positive      | half-infinity          |
| negative                                                                        | half-zero     |                        |
| half-floor                                                                      | positive      | half-zero              |
| negative                                                                        | half-infinity |                        |
| half-expand                                                                     | positive      | half-infinity          |
| negative                                                                        | half-infinity |                        |
| half-trunc                                                                      | positive      | half-zero              |
| negative                                                                        | half-zero     |                        |
| half-even                                                                       | positive      | half-even              |
| negative                                                                        | half-even     |                        |

# 13.28 ApplyUnsignedRoundingMode ( x, r1, r2, unsignedRoundingMode )

The abstract operation ApplyUnsignedRoundingMode takes arguments x (a mathematical value), r1 (a mathematical value), r2 (a mathematical value), and unsignedRoundingMode (either a specification type from the "Unsigned Rounding Mode" column of Table 22 or undefined) and returns a mathematical value. It considers x, bracketed below by r1 and above by r2, and returns either r1 or r2 according to unsignedRoundingMode. It performs the following steps when called:

1. If x = r1, return r1.
2. Assert: r1 < x < r2.
3. Assert: unsignedRoundingMode is not undefined.
4. If unsignedRoundingMode is zero, return r1.
5. If unsignedRoundingMode is infinity, return r2.
6. Let d1 be x – r1.
7. Let d2 be r2 – x.
8. If d1 < d2, return r1.
9. If d2 < d1, return r2.
10. Assert: d1 is equal to d2.
11. If unsignedRoundingMode is half-zero, return r1.
12. If unsignedRoundingMode is half-infinity, return r2.
13. Assert: unsignedRoundingMode is half-even.
14. Let cardinality be (r1 / (r2 – r1)) modulo 2.
15. If cardinality = 0, return r1.
16. Return r2.

# 13.29 RoundNumberToIncrement ( x, increment, roundingMode )

The abstract operation RoundNumberToIncrement takes arguments x (a mathematical value), increment (a positive integer), and roundingMode (a rounding mode) and returns an integer. It rounds x to the nearest multiple of increment, up or down according to roundingMode. It performs the following steps when called:

1. Let quotient be x / increment.
2. If quotient < 0, then
   1. Let isNegative be negative.
   2. Set quotient to -quotient.
3. Else,
   1. Let isNegative be positive.
4. Let unsignedRoundingMode be GetUnsignedRoundingMode(roundingMode, isNegative).
5. Let r1 be the largest integer such that r1 ≤ quotient.
6. Let r2 be the smallest integer such that r2 > quotient.
7. Let rounded be ApplyUnsignedRoundingMode(quotient, r1, r2, unsignedRoundingMode).
8. If isNegative is negative, set rounded to -rounded.
9. Return rounded × increment.

# 13.30 RoundNumberToIncrementAsIfPositive ( x, increment, roundingMode )

The abstract operation RoundNumberToIncrementAsIfPositive takes arguments x (a mathematical value), increment (a positive integer), and roundingMode (a rounding mode) and returns an integer. It rounds x to the nearest multiple of increment, up or down according to roundingMode, but always as if x were positive. For example, floor and trunc behave identically. This is used when rounding exact times, where "rounding down" conceptually always means towards the beginning of time, even if the time is expressed as a negative amount of time relative to an epoch. It performs the following steps when called:

1. Let quotient be x / increment.
2. Let unsignedRoundingMode be GetUnsignedRoundingMode(roundingMode, positive).
3. Let r1 be the largest integer such that r1 ≤ quotient.
4. Let r2 be the smallest integer such that r2 > quotient.
5. Let rounded be ApplyUnsignedRoundingMode(quotient, r1, r2, unsignedRoundingMode).
6. Return rounded × increment.

# 13.31 RFC 9557 / ISO 8601 grammar

Several operations in this section are intended to parse strings representing a date, a time, a duration, or a combined date and time. For the purposes of these operations, a valid ISO 8601 / RFC 9557 string is defined as a string that can be generated by one of the goal elements of the following grammar.

This grammar is adapted from the ABNF grammar of ISO 8601 that is given in appendix A of RFC 3339, augmented with the grammar of annotations in section 4.1 of RFC 9557

RFC 9557 and ISO 8601 are similar, but ISO 8601 defines a number of optional deviations that are allowed "by agreement between the communicating parties". The following is a list of deviations supported by this grammar:

- Only the calendar date format is supported, not the weekdate or ordinal date format.
- Two-digit years are disallowed.
- Expanded Years of 6 digits are allowed.
- Fractional parts may have 1 through 9 decimal places.
- In time representations, only seconds are allowed to have a fractional part.
- In duration representations, only hours, minutes, and seconds are allowed to have a fractional part.
- A space may be used to separate the date and time in a combined date / time representation, but not in a duration (e.g., "1970-01-01 00:00Z" is valid but "P1D 1H" is not).
- Alphabetic designators may be in lower or upper case (e.g., "1970-01-01t00:00Z" and "1970-01-01T00:00z" and "pT1m" are valid).
- Period or comma may be used as the decimal separator (e.g., "PT1,00H" is a valid representation of a 1-hour duration).
- UTC offsets of "-00:00" and "-0000" and "-00" are allowed, and all mean the same thing as "+00:00".
- UTC offsets may have seconds and up to 9 sub-second fractional digits (e.g., "1970-01-01T00:00:00+00:00:00.123456789" is valid).
- The constituent date, time, and UTC offset parts of a combined representation may each independently use basic format (with no separator symbols) or extended format (with mandatory `-` or `:` separators), as long as each such part is itself in either basic format or extended format (e.g., "1970-01-01T012345" and "19700101T01:23:45" are valid but "1970-0101T012345" and "1970-01-01T0123:45" are not).
- When parsing a date representation for a Temporal.PlainMonthDay, the year may be omitted. The year may optionally be replaced by `--` as in RFC 3339 Appendix A.
- When parsing a date representation without a day for a Temporal.PlainYearMonth, the expression is allowed to be in basic format (with no separator symbols).
- A duration specifier of "W" (weeks) can be combined with any of the other specifiers (e.g., "P1M1W1D" is valid).
- Anything else described by ISO 8601 as requiring mutual agreement between communicating parties, is disallowed.

In addition to the above deviations, any number of conforming RFC 9557 suffixes in square brackets are allowed. However, the only recognized suffixes are time zone and BCP 47 calendar. Others are ignored, unless they are prefixed with `!`, in which case they are rejected. Note that the suffix keys, although they look similar, are not the same as keys in RFC 6067. In particular, keys are lowercase-only.

Alpha ::: one of A B C D E F G H I J K L M N O P Q R S T U V W X Y Z a b c d e f g h i j k l m n o p q r s t u v w x y z LowercaseAlpha ::: one of a b c d e f g h i j k l m n o p q r s t u v w x y z DateSeparator[Extended] ::: [+Extended] - [~Extended] [empty] DaysDesignator ::: one of D d HoursDesignator ::: one of H h MinutesDesignator ::: one of M m MonthsDesignator ::: one of M m DurationDesignator ::: one of P p SecondsDesignator ::: one of S s DateTimeSeparator ::: <SP> T t TimeDesignator ::: one of T t WeeksDesignator ::: one of W w YearsDesignator ::: one of Y y UTCDesignator ::: one of Z z AnnotationCriticalFlag ::: ! DateYear ::: DecimalDigit DecimalDigit DecimalDigit DecimalDigit ASCIISign DecimalDigit DecimalDigit DecimalDigit DecimalDigit DecimalDigit DecimalDigit

Note the prohibition on negative zero in 13.31.3.

DateMonth ::: 0 NonZeroDigit 10 11 12 DateDay ::: 0 NonZeroDigit 1 DecimalDigit 2 DecimalDigit 30 31 DateSpecYearMonth ::: DateYear DateSeparator[+Extended] DateMonth DateYear DateSeparator[~Extended] DateMonth DateSpecMonthDay ::: \--opt DateMonth DateSeparator[+Extended] DateDay \--opt DateMonth DateSeparator[~Extended] DateDay

Note the prohibition on invalid combinations of month and day in 13.31.3.

DateSpec[Extended] ::: DateYear DateSeparator[?Extended] DateMonth DateSeparator[?Extended] DateDay

Note the prohibition on invalid combinations of month and day in 13.31.3.

Date ::: DateSpec[+Extended] DateSpec[~Extended] TimeSecond ::: MinuteSecond 60 NormalizedUTCOffset ::: ASCIISign Hour TimeSeparator[+Extended] MinuteSecond UTCOffset[SubMinutePrecision] ::: ASCIISign Hour ASCIISign Hour TimeSeparator[+Extended] MinuteSecond ASCIISign Hour TimeSeparator[~Extended] MinuteSecond [+SubMinutePrecision] ASCIISign Hour TimeSeparator[+Extended] MinuteSecond TimeSeparator[+Extended] MinuteSecond TemporalDecimalFractionopt [+SubMinutePrecision] ASCIISign Hour TimeSeparator[~Extended] MinuteSecond TimeSeparator[~Extended] MinuteSecond TemporalDecimalFractionopt DateTimeUTCOffset[Z] ::: [+Z] UTCDesignator UTCOffset[+SubMinutePrecision] TZLeadingChar ::: Alpha . _ TZChar ::: TZLeadingChar DecimalDigit - + TimeZoneIANANameComponent ::: TZLeadingChar TimeZoneIANANameComponent TZChar TimeZoneIANAName ::: TimeZoneIANANameComponent TimeZoneIANAName / TimeZoneIANANameComponent TimeZoneIdentifier ::: UTCOffset[~SubMinutePrecision] TimeZoneIANAName TimeZoneAnnotation ::: [ AnnotationCriticalFlagopt TimeZoneIdentifier ] AKeyLeadingChar ::: LowercaseAlpha _ AKeyChar ::: AKeyLeadingChar DecimalDigit - AnnotationKey ::: AKeyLeadingChar AnnotationKey AKeyChar AnnotationValueComponent ::: Alpha AnnotationValueComponentopt DecimalDigit AnnotationValueComponentopt AnnotationValue ::: AnnotationValueComponent AnnotationValueComponent - AnnotationValue Annotation ::: [ AnnotationCriticalFlagopt AnnotationKey = AnnotationValue ] Annotations ::: Annotation Annotationsopt TimeSpec[Extended] ::: Hour Hour TimeSeparator[?Extended] MinuteSecond Hour TimeSeparator[?Extended] MinuteSecond TimeSeparator[?Extended] TimeSecond TemporalDecimalFractionopt Time ::: TimeSpec[+Extended] TimeSpec[~Extended] DateTime[Z, TimeRequired] ::: [~TimeRequired] Date Date DateTimeSeparator Time DateTimeUTCOffset[?Z]opt AnnotatedTime ::: TimeDesignator Time DateTimeUTCOffset[~Z]opt TimeZoneAnnotationopt Annotationsopt Time DateTimeUTCOffset[~Z]opt TimeZoneAnnotationopt Annotationsopt

Note the disambiguation between the second alternative without TimeDesignator, and DateSpecMonthDay/DateSpecYearMonth in 13.31.3.

AnnotatedDateTime[Zoned, TimeRequired] ::: [~Zoned] DateTime[~Z, ?TimeRequired] TimeZoneAnnotationopt Annotationsopt [+Zoned] DateTime[+Z, ?TimeRequired] TimeZoneAnnotation Annotationsopt AnnotatedYearMonth ::: DateSpecYearMonth TimeZoneAnnotationopt Annotationsopt AnnotatedMonthDay ::: DateSpecMonthDay TimeZoneAnnotationopt Annotationsopt DurationSecondsPart ::: DecimalDigits[~Sep] TemporalDecimalFractionopt SecondsDesignator DurationMinutesPart ::: DecimalDigits[~Sep] TemporalDecimalFraction MinutesDesignator DecimalDigits[~Sep] MinutesDesignator DurationSecondsPartopt DurationHoursPart ::: DecimalDigits[~Sep] TemporalDecimalFraction HoursDesignator DecimalDigits[~Sep] HoursDesignator DurationMinutesPart DecimalDigits[~Sep] HoursDesignator DurationSecondsPartopt DurationTime ::: TimeDesignator DurationHoursPart TimeDesignator DurationMinutesPart TimeDesignator DurationSecondsPart DurationDaysPart ::: DecimalDigits[~Sep] DaysDesignator DurationWeeksPart ::: DecimalDigits[~Sep] WeeksDesignator DurationDaysPartopt DurationMonthsPart ::: DecimalDigits[~Sep] MonthsDesignator DurationWeeksPart DecimalDigits[~Sep] MonthsDesignator DurationDaysPartopt DurationYearsPart ::: DecimalDigits[~Sep] YearsDesignator DurationMonthsPart DecimalDigits[~Sep] YearsDesignator DurationWeeksPart DecimalDigits[~Sep] YearsDesignator DurationDaysPartopt DurationDate ::: DurationYearsPart DurationTimeopt DurationMonthsPart DurationTimeopt DurationWeeksPart DurationTimeopt DurationDaysPart DurationTimeopt Duration ::: ASCIISignopt DurationDesignator DurationDate ASCIISignopt DurationDesignator DurationTime TemporalInstantString ::: Date DateTimeSeparator Time DateTimeUTCOffset[+Z] TimeZoneAnnotationopt Annotationsopt TemporalDateTimeString[Zoned] ::: AnnotatedDateTime[?Zoned, ~TimeRequired] TemporalDurationString ::: Duration TemporalMonthDayString ::: AnnotatedMonthDay AnnotatedDateTime[~Zoned, ~TimeRequired] TemporalTimeString ::: AnnotatedTime AnnotatedDateTime[~Zoned, +TimeRequired] TemporalYearMonthString ::: AnnotatedYearMonth AnnotatedDateTime[~Zoned, ~TimeRequired]

# 13.31.1 Static Semantics: IsValidMonthDay

The syntax-directed operation IsValidMonthDay takes no arguments and returns a Boolean. It is defined piecewise over the following productions:

DateSpec[Extended] ::: DateYear DateSeparator[?Extended] DateMonth DateSeparator[?Extended] DateDay DateSpecMonthDay ::: \--opt DateMonth DateSeparator[+Extended] DateDay \--opt DateMonth DateSeparator[~Extended] DateDay

1. If DateDay is "31" and DateMonth is "02", "04", "06", "09", "11", return false.
2. If DateMonth is "02" and DateDay is "30", return false.
3. Return true.

# 13.31.2 Static Semantics: IsValidDate

The syntax-directed operation IsValidDate takes no arguments and returns a Boolean. It is defined piecewise over the following productions:

DateSpec[Extended] ::: DateYear DateSeparator[?Extended] DateMonth DateSeparator[?Extended] DateDay

1. If IsValidMonthDay of DateSpec is false, return false.
2. Let year be ℝ(StringToNumber(CodePointsToString(DateYear))).
3. If DateMonth is "02" and DateDay is "29" and MathematicalInLeapYear(EpochTimeForYear(year)) = 0, return false.
4. Return true.

# 13.31.3 Static Semantics: Early Errors

AnnotatedTime ::: Time DateTimeUTCOffset[~Z]opt TimeZoneAnnotationopt Annotationsopt

- It is a Syntax Error if ParseText(Time DateTimeUTCOffset[~Z], DateSpecMonthDay) is a Parse Node.
- It is a Syntax Error if ParseText(Time DateTimeUTCOffset[~Z], DateSpecYearMonth) is a Parse Node.

DateSpec[Extended] ::: DateYear DateSeparator[?Extended] DateMonth DateSeparator[?Extended] DateDay

- It is a Syntax Error if IsValidDate of DateSpec is false.

DateSpecMonthDay ::: \--opt DateMonth DateSeparator[+Extended] DateDay \--opt DateMonth DateSeparator[~Extended] DateDay

- It is a Syntax Error if IsValidMonthDay of DateSpecMonthDay is false.

DateYear ::: ASCIISign DecimalDigit DecimalDigit DecimalDigit DecimalDigit DecimalDigit DecimalDigit

- It is a Syntax Error if DateYear is "-000000".

# 13.32 ISO String Time Zone Parse Records

A ISO String Time Zone Parse Record is a Record value used to represent the result of parsing the representation of the time zone in an ISO 8601 / RFC 9557 string.

ISO String Time Zone Parse Records have the fields listed in Table 23.

| Table 23: ISO String Time Zone Parse Record Fields Field Name | Value             | Meaning                                                                 |
| ------------------------------------------------------------- | ----------------- | ----------------------------------------------------------------------- |
| [[Z]]                                                         | a Boolean         | Whether the string contained the `Z` UTC designator.                    |
| [[OffsetString]]                                              | a String or empty | The UTC offset from the string, or empty if none was present.           |
| [[TimeZoneAnnotation]]                                        | a String or empty | The time zone annotation from the string, or empty if none was present. |

# 13.33 Time Zone Identifier Parse Records

A Time Zone Identifier Parse Record is a Record value used to represent the result of parsing a time zone identifier either as an offset time zone or named time zone.

Time Zone Identifier Parse Records have the fields listed in Table 24.

The two fields are mutually exclusive. One of them always has the value empty.

| Table 24: Time Zone Identifier Parse Record Fields Field Name | Value                                                             | Meaning                                                                                                                           |
| ------------------------------------------------------------- | ----------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| [[Name]]                                                      | a String or empty                                                 | The time zone's name (not necessarily an available named time zone identifier), or empty if the time zone is an offset time zone. |
| [[OffsetMinutes]]                                             | an integer in the inclusive interval from -1439 to 1439, or empty | The time zone's UTC offset expressed as a number of minutes, or empty if the time zone is a named time zone.                      |

# 13.34 ISO Date-Time Parse Records

An ISO Date-Time Parse Record is a Record value used to represent the result of parsing an ISO 8601 / RFC 9557 string.

For any ISO Date-Time Parse Record r, IsValidISODate(r.[[Year]], r.[[Month]], r.[[Day]]) must return true, or, if r.[[Year]] is empty, IsValidISODate(1972, r.[[Month]], r.[[Day]]) must return true. It is not necessary for the represented date and time to be within the range given by ISODateTimeWithinLimits.

ISO Date-Time Parse Records have the fields listed in Table 25.

| Table 25: ISO Date-Time Parse Record Fields Field Name | Value                                                       | Meaning                                                                                                                         |
| ------------------------------------------------------ | ----------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| [[Year]]                                               | an integer or empty                                         | The year in the ISO 8601 calendar, or empty if the string's format was that of TemporalMonthDayString and the year was omitted. |
| [[Month]]                                              | an integer between 1 and 12, inclusive                      | The number of the month in the ISO 8601 calendar.                                                                               |
| [[Day]]                                                | an integer between 1 and 31, inclusive                      | The number of the day of the month in the ISO 8601 calendar.                                                                    |
| [[Time]]                                               | either a Time Record with [[Days]] value 0, or start-of-day | The time of day, or start-of-day if the time was omitted from the string.                                                       |
| [[TimeZone]]                                           | an ISO String Time Zone Parse Record                        | A representation of how the time zone was expressed in the string.                                                              |
| [[Calendar]]                                           | a String or empty                                           | The calendar type from the string, or empty if none was present.                                                                |

# 13.35 ParseISODateTime ( isoString, allowedFormats )

The abstract operation ParseISODateTime takes arguments isoString (a String) and allowedFormats (a List of nonterminals) and returns either a normal completion containing an ISO Date-Time Parse Record or a throw completion. It parses the argument as an ISO 8601 / RFC 9557 string and returns a Record representing each date and time component as a distinct field. It performs the following steps when called:

1. Let parseResult be empty.
2. Let calendar be empty.
3. Let yearAbsent be false.
4. For each nonterminal goal of allowedFormats, do
   1. If parseResult is not a Parse Node, then
      1. Set parseResult to ParseText(StringToCodePoints(isoString), goal).
      2. If parseResult is a Parse Node, then
         1. Let calendarWasCritical be false.
         2. For each Annotation Parse Node annotation contained within parseResult, do
            1. Let key be the source text matched by the AnnotationKey Parse Node contained within annotation.
            2. Let value be the source text matched by the AnnotationValue Parse Node contained within annotation.
            3. If CodePointsToString(key) is "u-ca", then
               1. If calendar is empty, then
                  1. Set calendar to CodePointsToString(value).
                  2. If annotation contains an AnnotationCriticalFlag Parse Node, set calendarWasCritical to true.
               2. Else,
                  1. If annotation contains an AnnotationCriticalFlag Parse Node, or calendarWasCritical is true, throw a RangeError exception.
            4. Else,
               1. If annotation contains an AnnotationCriticalFlag Parse Node, throw a RangeError exception.
         3. If goal is TemporalYearMonthString and parseResult does not contain a DateDay Parse Node, then
            1. If calendar is not empty and the ASCII-lowercase of calendar is not "iso8601", throw a RangeError exception.
         4. If goal is TemporalMonthDayString and parseResult does not contain a DateYear Parse Node, then
            1. If calendar is not empty and the ASCII-lowercase of calendar is not "iso8601", throw a RangeError exception.
            2. Set yearAbsent to true.
5. If parseResult is not a Parse Node, throw a RangeError exception.
6. NOTE: Applications of StringToNumber below do not lose precision, since each of the parsed values is guaranteed to be a sufficiently short string of decimal digits.
7. Let each of year, month, day, hour, minute, second, and fSeconds be the source text matched by the respective DateYear, DateMonth, DateDay, the first Hour, the first MinuteSecond, TimeSecond, and the first TemporalDecimalFraction Parse Node contained within parseResult, or an empty sequence of code points if not present.
8. Let yearMV be ℝ(StringToNumber(CodePointsToString(year))).
9. If month is empty, then
   1. Let monthMV be 1.
10. Else,
    1. Let monthMV be ℝ(StringToNumber(CodePointsToString(month))).
11. If day is empty, then
    1. Let dayMV be 1.
12. Else,
    1. Let dayMV be ℝ(StringToNumber(CodePointsToString(day))).
13. If hour is empty, then
    1. Let hourMV be 0.
14. Else,
    1. Let hourMV be ℝ(StringToNumber(CodePointsToString(hour))).
15. If minute is empty, then
    1. Let minuteMV be 0.
16. Else,
    1. Let minuteMV be ℝ(StringToNumber(CodePointsToString(minute))).
17. If second is empty, then
    1. Let secondMV be 0.
18. Else,
    1. Let secondMV be ℝ(StringToNumber(CodePointsToString(second))).
    2. If secondMV = 60, then
    3. Set secondMV to 59.
19. If fSeconds is not empty, then
    1. Let fSecondsDigits be the substring of CodePointsToString(fSeconds) from 1.
    2. Let fSecondsDigitsExtended be the string-concatenation of fSecondsDigits and "000000000".
    3. Let millisecond be the substring of fSecondsDigitsExtended from 0 to 3.
    4. Let microsecond be the substring of fSecondsDigitsExtended from 3 to 6.
    5. Let nanosecond be the substring of fSecondsDigitsExtended from 6 to 9.
    6. Let millisecondMV be ℝ(StringToNumber(millisecond)).
    7. Let microsecondMV be ℝ(StringToNumber(microsecond)).
    8. Let nanosecondMV be ℝ(StringToNumber(nanosecond)).
20. Else,
    1. Let millisecondMV be 0.
    2. Let microsecondMV be 0.
    3. Let nanosecondMV be 0.
21. Assert: IsValidISODate(yearMV, monthMV, dayMV) is true.
22. If hour is empty, then
    1. Let time be start-of-day.
23. Else,
    1. Let time be CreateTimeRecord(hourMV, minuteMV, secondMV, millisecondMV, microsecondMV, nanosecondMV).
24. Let timeZoneResult be ISO String Time Zone Parse Record { [[Z]]: false, [[OffsetString]]: empty, [[TimeZoneAnnotation]]: empty }.
25. If parseResult contains a TimeZoneIdentifier Parse Node, then
    1. Let identifier be the source text matched by the TimeZoneIdentifier Parse Node contained within parseResult.
    2. Set timeZoneResult.[[TimeZoneAnnotation]] to CodePointsToString(identifier).
26. If parseResult contains a UTCDesignator Parse Node, then
    1. Set timeZoneResult.[[Z]] to true.
27. Else if parseResult contains a UTCOffset[+SubMinutePrecision] Parse Node, then
    1. Let offset be the source text matched by the UTCOffset[+SubMinutePrecision] Parse Node contained within parseResult.
    2. Set timeZoneResult.[[OffsetString]] to CodePointsToString(offset).
28. If yearAbsent is true, let yearReturn be empty; else let yearReturn be yearMV.
29. Return ISO Date-Time Parse Record { [[Year]]: yearReturn, [[Month]]: monthMV, [[Day]]: dayMV, [[Time]]: time, [[TimeZone]]: timeZoneResult, [[Calendar]]: calendar }.

# 13.36 ParseTemporalCalendarString ( string )

The abstract operation ParseTemporalCalendarString takes argument string (a String) and returns either a normal completion containing a String or a throw completion. It parses the argument either as an ISO 8601 / RFC 9557 string or bare calendar type, and returns the calendar type. The returned string is syntactically a valid calendar type, but not necessarily an existing one. It performs the following steps when called:

1. Let parseResult be Completion(ParseISODateTime(string, « TemporalDateTimeString[+Zoned], TemporalDateTimeString[~Zoned], TemporalInstantString, TemporalTimeString, TemporalMonthDayString, TemporalYearMonthString »)).
2. If parseResult is a normal completion, then
   1. Let calendar be parseResult.[[Value]].[[Calendar]].
   2. If calendar is empty, return "iso8601".
   3. Return calendar.
3. Set parseResult to ParseText(StringToCodePoints(string), AnnotationValue).
4. If parseResult is a List of errors, throw a RangeError exception.
5. Return string.

# 13.37 ParseTemporalDurationString ( isoString )

The abstract operation ParseTemporalDurationString takes argument isoString (a String) and returns either a normal completion containing a Temporal.Duration or a throw completion. It parses the argument as an ISO 8601 duration string.

Note 1

ToIntegerWithTruncation(the empty String) = 0.

Note 2

Use of mathematical values rather than approximations is important to avoid off-by-one errors with input like "PT46H66M71.50040904S".

It performs the following steps when called:

1. Let duration be ParseText(StringToCodePoints(isoString), TemporalDurationString).
2. If duration is a List of errors, throw a RangeError exception.
3. Let sign be the source text matched by the ASCIISign Parse Node contained within duration, or an empty sequence of code points if not present.
4. If duration contains a DurationYearsPart Parse Node, then
   1. Let yearsNode be that DurationYearsPart Parse Node contained within duration.
   2. Let years be the source text matched by the DecimalDigits Parse Node contained within yearsNode.
5. Else,
   1. Let years be an empty sequence of code points.
6. If duration contains a DurationMonthsPart Parse Node, then
   1. Let monthsNode be the DurationMonthsPart Parse Node contained within duration.
   2. Let months be the source text matched by the DecimalDigits Parse Node contained within monthsNode.
7. Else,
   1. Let months be an empty sequence of code points.
8. If duration contains a DurationWeeksPart Parse Node, then
   1. Let weeksNode be the DurationWeeksPart Parse Node contained within duration.
   2. Let weeks be the source text matched by the DecimalDigits Parse Node contained within weeksNode.
9. Else,
   1. Let weeks be an empty sequence of code points.
10. If duration contains a DurationDaysPart Parse Node, then
    1. Let daysNode be the DurationDaysPart Parse Node contained within duration.
    2. Let days be the source text matched by the DecimalDigits Parse Node contained within daysNode.
11. Else,
    1. Let days be an empty sequence of code points.
12. If duration contains a DurationHoursPart Parse Node, then
    1. Let hoursNode be the DurationHoursPart Parse Node contained within duration.
    2. Let hours be the source text matched by the DecimalDigits Parse Node contained within hoursNode.
    3. Let fHours be the source text matched by the TemporalDecimalFraction Parse Node contained within hoursNode, or an empty sequence of code points if not present.
13. Else,
    1. Let hours be an empty sequence of code points.
    2. Let fHours be an empty sequence of code points.
14. If duration contains a DurationMinutesPart Parse Node, then
    1. Let minutesNode be the DurationMinutesPart Parse Node contained within duration.
    2. Let minutes be the source text matched by the DecimalDigits Parse Node contained within minutesNode.
    3. Let fMinutes be the source text matched by the TemporalDecimalFraction Parse Node contained within minutesNode, or an empty sequence of code points if not present.
15. Else,
    1. Let minutes be an empty sequence of code points.
    2. Let fMinutes be an empty sequence of code points.
16. If duration contains a DurationSecondsPart Parse Node, then
    1. Let secondsNode be the DurationSecondsPart Parse Node contained within duration.
    2. Let seconds be the source text matched by the DecimalDigits Parse Node contained within secondsNode.
    3. Let fSeconds be the source text matched by the TemporalDecimalFraction Parse Node contained within secondsNode, or an empty sequence of code points if not present.
17. Else,
    1. Let seconds be an empty sequence of code points.
    2. Let fSeconds be an empty sequence of code points.
18. Let yearsMV be ? ToIntegerWithTruncation(CodePointsToString(years)).
19. Let monthsMV be ? ToIntegerWithTruncation(CodePointsToString(months)).
20. Let weeksMV be ? ToIntegerWithTruncation(CodePointsToString(weeks)).
21. Let daysMV be ? ToIntegerWithTruncation(CodePointsToString(days)).
22. Let hoursMV be ? ToIntegerWithTruncation(CodePointsToString(hours)).
23. If fHours is not empty, then
    1. Assert: minutes, fMinutes, seconds, and fSeconds are empty.
    2. Let fHoursDigits be the substring of CodePointsToString(fHours) from 1.
    3. Let fHoursScale be the length of fHoursDigits.
    4. Let minutesMV be ? ToIntegerWithTruncation(fHoursDigits) / 10fHoursScale × 60.
24. Else,
    1. Let minutesMV be ? ToIntegerWithTruncation(CodePointsToString(minutes)).
25. If fMinutes is not empty, then
    1. Assert: seconds and fSeconds are empty.
    2. Let fMinutesDigits be the substring of CodePointsToString(fMinutes) from 1.
    3. Let fMinutesScale be the length of fMinutesDigits.
    4. Let secondsMV be ? ToIntegerWithTruncation(fMinutesDigits) / 10fMinutesScale × 60.
26. Else if seconds is not empty, then
    1. Let secondsMV be ? ToIntegerWithTruncation(CodePointsToString(seconds)).
27. Else,
    1. Let secondsMV be remainder(minutesMV, 1) × 60.
28. If fSeconds is not empty, then
    1. Let fSecondsDigits be the substring of CodePointsToString(fSeconds) from 1.
    2. Let fSecondsScale be the length of fSecondsDigits.
    3. Let millisecondsMV be ? ToIntegerWithTruncation(fSecondsDigits) / 10fSecondsScale × 1000.
29. Else,
    1. Let millisecondsMV be remainder(secondsMV, 1) × 1000.
30. Let microsecondsMV be remainder(millisecondsMV, 1) × 1000.
31. Let nanosecondsMV be remainder(microsecondsMV, 1) × 1000.
32. If sign contains the code point U+002D (HYPHEN-MINUS), then
    1. Let factor be -1.
33. Else,
    1. Let factor be 1.
34. Set yearsMV to yearsMV × factor.
35. Set monthsMV to monthsMV × factor.
36. Set weeksMV to weeksMV × factor.
37. Set daysMV to daysMV × factor.
38. Set hoursMV to hoursMV × factor.
39. Set minutesMV to floor(minutesMV) × factor.
40. Set secondsMV to floor(secondsMV) × factor.
41. Set millisecondsMV to floor(millisecondsMV) × factor.
42. Set microsecondsMV to floor(microsecondsMV) × factor.
43. Set nanosecondsMV to floor(nanosecondsMV) × factor.
44. Return ? CreateTemporalDuration(yearsMV, monthsMV, weeksMV, daysMV, hoursMV, minutesMV, secondsMV, millisecondsMV, microsecondsMV, nanosecondsMV).

# 13.38 ParseTemporalTimeZoneString ( timeZoneString )

The abstract operation ParseTemporalTimeZoneString takes argument timeZoneString (a String) and returns either a normal completion containing a Time Zone Identifier Parse Record, or a throw completion. It parses the argument as either a time zone identifier or an ISO 8601 / RFC 9557 string. The returned Record's fields are set as follows:

- If timeZoneString is a named time zone identifier, then [[Name]] is timeZoneString.
- Otherwise, if timeZoneString is an offset time zone identifier, then [[OffsetMinutes]] is the time zone's UTC offset.
- Otherwise, if timeZoneString is a string with a time zone annotation containing a named time zone identifier, then [[Name]] is the time zone identifier contained in the annotation.
- Otherwise, if timeZoneString is a string with a time zone annotation containing an offset time zone identifier, then [[OffsetMinutes]] is the time zone's UTC offset.
- Otherwise, if timeZoneString is a string using a Z offset designator, then [[Name]] is "UTC".
- Otherwise, if timeZoneString is a string using a numeric UTC offset, then [[OffsetMinutes]] is that numeric UTC offset.
- Otherwise, a RangeError is thrown.

It performs the following steps when called:

1. Let parseResult be ParseText(StringToCodePoints(timeZoneString), TimeZoneIdentifier).
2. If parseResult is a Parse Node, return ! ParseTimeZoneIdentifier(timeZoneString).
3. Let result be ? ParseISODateTime(timeZoneString, « TemporalDateTimeString[+Zoned], TemporalDateTimeString[~Zoned], TemporalInstantString, TemporalTimeString, TemporalMonthDayString, TemporalYearMonthString »).
4. Let timeZoneResult be result.[[TimeZone]].
5. If timeZoneResult.[[TimeZoneAnnotation]] is not empty, return ! ParseTimeZoneIdentifier(timeZoneResult.[[TimeZoneAnnotation]]).
6. If timeZoneResult.[[Z]] is true, return ! ParseTimeZoneIdentifier("UTC").
7. If timeZoneResult.[[OffsetString]] is not empty, return ? ParseTimeZoneIdentifier(timeZoneResult.[[OffsetString]]).
8. Throw a RangeError exception.

# 13.39 ToPositiveIntegerWithTruncation ( argument )

The abstract operation ToPositiveIntegerWithTruncation takes argument argument (an ECMAScript language value) and returns either a normal completion containing a positive integer or a throw completion. It converts argument to an integer representing its Number value with fractional part truncated, or throws a RangeError when that value is not finite or not positive. It performs the following steps when called:

1. Let integer be ? ToIntegerWithTruncation(argument).
2. If integer ≤ 0, throw a RangeError exception.
3. Return integer.

# 13.40 ToIntegerWithTruncation ( argument )

The abstract operation ToIntegerWithTruncation takes argument argument (an ECMAScript language value) and returns either a normal completion containing an integer or a throw completion. It converts argument to an integer representing its Number value with fractional part truncated, or throws a RangeError when that value is not finite. It performs the following steps when called:

1. Let number be ? ToNumber(argument).
2. If number is one of NaN, +∞𝔽, or -∞𝔽, throw a RangeError exception.
3. Return truncate(ℝ(number)).

# 13.41 ToOffsetString ( argument )

The abstract operation ToOffsetString takes argument argument (an ECMAScript language value) and returns either a normal completion containing a String or a throw completion. It converts argument to a String, or throws a TypeError if that is not possible. It also requires that the String is parseable as a UTC offset string, or throws a RangeError if it is not. It performs the following steps when called:

1. Let offset be ? ToPrimitive(argument, string).
2. If offset is not a String, throw a TypeError exception.
3. Perform ? ParseDateTimeUTCOffset(offset).
4. Return offset.

# 13.42 ISODateToFields ( calendar, isoDate, type )

The abstract operation ISODateToFields takes arguments calendar (a calendar type), isoDate (an ISO Date Record), and type (one of date, year-month, or month-day) and returns a Calendar Fields Record. It converts isoDate to a Calendar Fields Record, populating different fields depending on type. It performs the following steps when called:

1. Let fields be an empty Calendar Fields Record with all fields set to unset.
2. Let calendarDate be CalendarISOToDate(calendar, isoDate).
3. Set fields.[[MonthCode]] to calendarDate.[[MonthCode]].
4. If type is either month-day or date, then
   1. Set fields.[[Day]] to calendarDate.[[Day]].
5. If type is either year-month or date, then
   1. Set fields.[[Year]] to calendarDate.[[Year]].
6. Return fields.

# 13.43 GetDifferenceSettings ( operation, options, unitGroup, disallowedUnits, fallbackSmallestUnit, smallestLargestDefaultUnit )

The abstract operation GetDifferenceSettings takes arguments operation (either since or until), options (an Object), unitGroup (one of date, time, or datetime), disallowedUnits (a List of Temporal units), fallbackSmallestUnit (a Temporal unit), and smallestLargestDefaultUnit (a Temporal unit) and returns either a normal completion containing a Record with fields [[SmallestUnit]] (a Temporal unit), [[LargestUnit]] (a Temporal unit), [[RoundingMode]] (a rounding mode), and [[RoundingIncrement]] (an integer in the inclusive interval from 1 to 109), or a throw completion. It reads unit and rounding options needed by difference operations. It performs the following steps when called:

1. NOTE: The following steps read options and perform independent validation in alphabetical order.
2. Let largestUnit be ? GetTemporalUnitValuedOption(options, "largestUnit", unset).
3. Let roundingIncrement be ? GetRoundingIncrementOption(options).
4. Let roundingMode be ? GetRoundingModeOption(options, trunc).
5. Let smallestUnit be ? GetTemporalUnitValuedOption(options, "smallestUnit", unset).
6. Perform ? ValidateTemporalUnitValue(largestUnit, unitGroup, « auto »).
7. If largestUnit is unset, then
   1. Set largestUnit to auto.
8. If disallowedUnits contains largestUnit, throw a RangeError exception.
9. Perform ? ValidateTemporalUnitValue(smallestUnit, unitGroup).
10. If smallestUnit is unset, then
    1. Set smallestUnit to fallbackSmallestUnit.
11. If disallowedUnits contains smallestUnit, throw a RangeError exception.
12. Let defaultLargestUnit be LargerOfTwoTemporalUnits(smallestLargestDefaultUnit, smallestUnit).
13. If largestUnit is auto, set largestUnit to defaultLargestUnit.
14. If LargerOfTwoTemporalUnits(largestUnit, smallestUnit) is not largestUnit, throw a RangeError exception.
15. Let maximum be MaximumTemporalDurationRoundingIncrement(smallestUnit).
16. If maximum is not unset, perform ? ValidateTemporalRoundingIncrement(roundingIncrement, maximum, false).
17. If operation is since, then
    1. Set roundingMode to NegateRoundingMode(roundingMode).
18. Return the Record { [[SmallestUnit]]: smallestUnit, [[LargestUnit]]: largestUnit, [[RoundingMode]]: roundingMode, [[RoundingIncrement]]: roundingIncrement, }.

# 14 Amendments to the ECMAScript® 2023 Language Specification

Editor's Note

This section lists amendments which must be made to ECMA-262, the ECMAScript® 2023 Language Specification, other than the addition of the new sections specifying the Temporal object and everything related to it. Text to be added is marked like this, and text to be deleted is marked ~~like this~~. Blocks of unmodified text between modified sections are marked by [...].

# 14.1 The String Type

Editor's Note

This section intends to move the definitions of ASCII-uppercase, ASCII-lowercase, and ASCII-case-insensitive match from ECMA-402 into ECMA-262, after the definition of the ASCII word characters.

[...]

The ASCII-uppercase of a String S is the String derived from S by replacing each occurrence of an ASCII lowercase letter code unit (0x0061 through 0x007A, inclusive) with the corresponding ASCII uppercase letter code unit (0x0041 through 0x005A, inclusive) while preserving all other code units.

The ASCII-lowercase of a String S is the String derived from S by replacing each occurrence of an ASCII uppercase letter code unit (0x0041 through 0x005A, inclusive) with the corresponding ASCII lowercase letter code unit (0x0061 through 0x007A, inclusive) while preserving all other code units.

A String A is an ASCII-case-insensitive match for a String B if the ASCII-lowercase of A is the ASCII-lowercase of B.

# 14.2 The Object Type

# 14.2.1 Well-Known Intrinsic Objects

| Table 26: Well-Known Intrinsic Objects %Temporal% | `Temporal` | The `Temporal` object (1) |
| ------------------------------------------------- | ---------- | ------------------------- |

# 14.3 The Year-Week Record Specification Type

The Year-Week Record specification type is returned by the week number calculation in ISOWeekOfYear, and the corresponding calculations for other calendars if applicable. It comprises a _calendar week of year_ with the corresponding _week calendar year_.

Year-Week Records have the fields listed in table Table 27.

| Table 27: Year-Week Record Fields Field Name | Value                           | Meaning                    |
| -------------------------------------------- | ------------------------------- | -------------------------- |
| [[Week]]                                     | a positive integer or undefined | The calendar week of year. |
| [[Year]]                                     | an integer or undefined         | The week calendar year.    |

# 14.4 Mathematical Operations

[...]

The notation “x modulo y” (y must be finite and non-zero) computes a value k of the same sign as y (or zero) such that abs(k) < abs(y) and x \- k = q × y for some integer q.

The mathematical function remainder(x, y) produces the mathematical value whose sign is the sign of x and whose magnitude is abs(x) modulo y.

[...]

Mathematical functions min, max, abs, remainder, floor, and truncate are not defined for Numbers and BigInts, and any usage of those methods that have non-mathematical value arguments would be an editorial error in this specification.

[...]

# 14.5 Abstract Operations

# 14.5.1 Type Conversion

[...]

# 14.5.1.1 ToIntegerIfIntegral ( argument )

The abstract operation ToIntegerIfIntegral takes argument argument (an ECMAScript language value) and returns either a normal completion containing an integer or a throw completion. It converts argument to an integer representing its Number value, or throws a RangeError when that value is not integral. It performs the following steps when called:

1. Let number be ? ToNumber(argument).
2. If number is not an integral Number, throw a RangeError exception.
3. Return ℝ(number).

[...]

# 14.5.2 Operations for Reading Options

# 14.5.2.1 GetOption ( options, property, type, values, default )

The abstract operation GetOption takes arguments options (an Object), property (a property key), type (either boolean or string), values (either a List of ECMAScript language values, or empty), and default (either an ECMAScript language value or required) and returns either a normal completion containing an ECMAScript language value or a throw completion. It extracts the value of the specified property of options, converts it to the required type, checks whether it is allowed by values if values is not empty, and substitutes default if the value is undefined. It performs the following steps when called:

1. Let value be ? Get(options, property).
2. If value is undefined, then
   1. If default is required, throw a RangeError exception.
   2. Return default.
3. If type is boolean, then
   1. Set value to ToBoolean(value).
4. Else,
   1. Assert: type is string.
   2. Set value to ? ToString(value).
5. If values is not empty and values does not contain value, throw a RangeError exception.
6. Return value.

# 14.5.2.2 GetRoundingModeOption ( options, fallback )

The abstract operation GetRoundingModeOption takes arguments options (an Object) and fallback (a rounding mode) and returns either a normal completion containing a rounding mode, or a throw completion. It fetches and validates the "roundingMode" property from options, returning fallback as a default if absent. It performs the following steps when called:

1. Let allowedStrings be the List of Strings from the "String Identifier" column of Table 28.
2. Let stringFallback be the value from the "String Identifier" column of the row with fallback in its "Rounding Mode" column.
3. Let stringValue be ? GetOption(options, "roundingMode", string, allowedStrings, stringFallback).
4. Return the value from the "Rounding Mode" column of the row with stringValue in its "String Identifier" column.

A rounding mode is one of the values in the "Rounding Mode" column of Table 28. An unsigned rounding mode is one of the values in the "Unsigned Rounding Mode" column of Table 22.

Editor's Note

The following table is taken from ECMA-402 (Table 29) but with the addition of specification values.

| Table 28: Rounding modes Rounding Mode | String Identifier | Description                                     | Examples: Round to 0 fraction digits |
| -------------------------------------- | ----------------- | ----------------------------------------------- | ------------------------------------ |
| -1.5                                   | 0.4               | 0.5                                             | 0.6                                  |
| ceil                                   | "ceil"            | Toward positive infinity                        | ⬆️ [-1]                              |
| floor                                  | "floor"           | Toward negative infinity                        | ⬇️ [-2]                              |
| expand                                 | "expand"          | Away from zero                                  | ⬇️ [-2]                              |
| trunc                                  | "trunc"           | Toward zero                                     | ⬆️ [-1]                              |
| half-ceil                              | "halfCeil"        | Ties toward positive infinity                   | ⬆️ [-1]                              |
| half-floor                             | "halfFloor"       | Ties toward negative infinity                   | ⬇️ [-2]                              |
| half-expand                            | "halfExpand"      | Ties away from zero                             | ⬇️ [-2]                              |
| half-trunc                             | "halfTrunc"       | Ties toward zero                                | ⬆️ [-1]                              |
| half-even                              | "halfEven"        | Ties toward an even rounding increment multiple | ⬇️ [-2]                              |
| Note                                   |                   |                                                 |                                      |

The examples are illustrative of the unique behaviour of each option. ⬆️ means "resolves toward positive infinity"; ⬇️ means "resolves toward negative infinity".

# 14.5.2.3 GetRoundingIncrementOption ( options )

The abstract operation GetRoundingIncrementOption takes argument options (an Object) and returns either a normal completion containing a positive integer in the inclusive interval from 1 to 109, or a throw completion. It fetches and validates the "roundingIncrement" property from options, returning a default if absent. It performs the following steps when called:

1. Let value be ? Get(options, "roundingIncrement").
2. If value is undefined, return 1𝔽.
3. Let integerIncrement be ? ToIntegerWithTruncation(value).
4. If integerIncrement < 1 or integerIncrement > 109, throw a RangeError exception.
5. Return integerIncrement.

# 14.6 Overview of Date Objects and Definitions of Abstract Operations

[...]

# 14.6.1 GetUTCEpochNanoseconds ( ~~year~~ , ~~month~~ , ~~day~~ , ~~hour~~ , ~~minute~~ , ~~second~~ , ~~millisecond~~ , ~~microsecond~~ , ~~nanosecond~~ , isoDateTime )

The abstract operation GetUTCEpochNanoseconds takes arguments ~~year (an integer)~~, ~~month (an integer in the inclusive interval from 1 to 12)~~, ~~day (an integer in the inclusive interval from 1 to 31)~~, ~~hour (an integer in the inclusive interval from 0 to 23)~~, ~~minute (an integer in the inclusive interval from 0 to 59)~~, ~~second (an integer in the inclusive interval from 0 to 59)~~, ~~millisecond (an integer in the inclusive interval from 0 to 999)~~, ~~microsecond (an integer in the inclusive interval from 0 to 999)~~, ~~nanosecond (an integer in the inclusive interval from 0 to 999)~~, and isoDateTime (an ISO Date-Time Record) and returns a BigInt. The returned value represents a number of nanoseconds since the epoch that corresponds to the given ISO 8601 calendar date and wall-clock time in UTC. It performs the following steps when called:

1. Let date be MakeDay(𝔽(~~year~~ isoDateTime.[[ISODate]].[[Year]]), 𝔽(~~month~~ isoDateTime.[[ISODate]].[[Month]] \- 1), 𝔽(~~day~~ isoDateTime.[[ISODate]].[[Day]])).
2. Let time be MakeTime(𝔽(~~hour~~ isoDateTime.[[Time]].[[Hour]]), 𝔽(~~minute~~ isoDateTime.[[Time]].[[Minute]]), 𝔽(~~second~~ isoDateTime.[[Time]].[[Second]]), 𝔽(~~millisecond~~ isoDateTime.[[Time]].[[Millisecond]])).
3. Let ms be MakeDate(date, time).
4. Assert: ms is an integral Number.
5. Return ℤ(ℝ(ms) × 106 \+ ~~microsecond~~ isoDateTime.[[Time]].[[Microsecond]] × 103 \+ ~~nanosecond~~ isoDateTime.[[Time]].[[Nanosecond]]).

# 14.6.2 Time Zone Identifiers

Time zones in ECMAScript are represented by time zone identifiers, which are Strings composed entirely of code units in the inclusive interval from ~~0x0000 to 0x007F~~ 0x0021 to 0x007E. Time zones supported by an ECMAScript implementation may be available named time zones, represented by the [[Identifier]] field of the Time Zone Identifier Records returned by AvailableNamedTimeZoneIdentifiers, or offset time zones, represented by Strings for which ~~IsTimeZoneOffsetString~~IsOffsetTimeZoneIdentifier returns true.

A primary time zone identifier is the preferred identifier for an available named time zone. A non-primary time zone identifier is an identifier for an available named time zone that is not a primary time zone identifier. An available named time zone identifier is either a primary time zone identifier or a non-primary time zone identifier. Each available named time zone identifier is associated with exactly one available named time zone. Each available named time zone is associated with exactly one primary time zone identifier and zero or more non-primary time zone identifiers.

An available time zone identifier is either an available named time zone identifier or an offset time zone identifier.

Time zone identifiers are compared using ASCII-case-insensitive comparisons, and are accepted as input in any variation of letter case. Offset time zone identifiers are compared using the number of minutes represented (not as a String), and are accepted as input in any the formats specified by UTCOffset[~SubMinutePrecision]. However, ECMAScript built-in objects will only output the normalized format of a time zone identifier. The normalized format of an available named time zone identifier is the preferred letter case for that identifier. The normalized format of an offset time zone identifier is specified by NormalizedUTCOffset and produced by FormatOffsetTimeZoneIdentifier with style either not present or set to separated.

ECMAScript implementations must support an available named time zone with the identifier "UTC", which must be the primary time zone identifier for the UTC time zone. In addition, implementations may support any number of other available named time zones.

Implementations that follow the requirements for time zones as described in the ECMA-402 Internationalization API specification are called time zone aware. Time zone aware implementations must support available named time zones corresponding to the Zone and Link names of the IANA Time Zone Database, and only such names. In time zone aware implementations, a primary time zone identifier is a Zone name, and a non-primary time zone identifier is a Link name, respectively, in the IANA Time Zone Database except as specifically overridden by AvailableNamedTimeZoneIdentifiers as specified in the ECMA-402 specification. Implementations that do not support the entire IANA Time Zone Database are still recommended to use IANA Time Zone Database names as identifiers to represent time zones.

# 14.6.3 GetNamedTimeZoneEpochNanoseconds ( timeZoneIdentifier, ~~year~~ , ~~month~~ , ~~day~~ , ~~hour~~ , ~~minute~~ , ~~second~~ , ~~millisecond~~ , ~~microsecond~~ , ~~nanosecond~~ , isoDateTime )

The implementation-defined abstract operation GetNamedTimeZoneEpochNanoseconds takes arguments timeZoneIdentifier (a String), ~~year (an integer)~~, ~~month (an integer in the inclusive interval from 1 to 12)~~, ~~day (an integer in the inclusive interval from 1 to 31)~~, ~~hour (an integer in the inclusive interval from 0 to 23)~~, ~~minute (an integer in the inclusive interval from 0 to 59)~~, ~~second (an integer in the inclusive interval from 0 to 59)~~, ~~millisecond (an integer in the inclusive interval from 0 to 999)~~, ~~microsecond (an integer in the inclusive interval from 0 to 999)~~, ~~nanosecond (an integer in the inclusive interval from 0 to 999)~~, and isoDateTime (an ISO Date-Time Record) and returns a List of BigInts. Each value in the returned List represents a number of nanoseconds since the epoch that corresponds to the given ISO 8601 calendar date and wall-clock time in the named time zone identified by timeZoneIdentifier.

When the input represents a local time occurring more than once because of a negative time zone transition (e.g. when daylight saving time ends or the time zone offset is decreased due to a time zone rule change), the returned List will have more than one element and will be sorted by ascending numerical value. When the input represents a local time skipped because of a positive time zone transition (e.g. when daylight saving time begins or the time zone offset is increased due to a time zone rule change), the returned List will be empty. Otherwise, the returned List will have one element.

The default implementation of GetNamedTimeZoneEpochNanoseconds, to be used for ECMAScript implementations that do not include local political rules for any time zones, performs the following steps when called:

1. Assert: timeZoneIdentifier is "UTC".
2. Let epochNanoseconds be GetUTCEpochNanoseconds(~~year , month, day, hour, minute, second, millisecond, microsecond, nanosecond~~isoDateTime).
3. Return « epochNanoseconds ».

Note

It is required for time zone aware implementations (and recommended for all others) to use the time zone information of the IANA Time Zone Database https://www.iana.org/time-zones/.

1:30 AM on 5 November 2017 in America/New_York is repeated twice, so GetNamedTimeZoneEpochNanoseconds~~( "America/New_York", 2017, 11, 5, 1, 30, 0, 0, 0, 0)~~ for that time zone and ISO date-time would return a List of length 2 in which the first element represents 05:30 UTC (corresponding with 01:30 US Eastern Daylight Time at UTC offset -04:00) and the second element represents 06:30 UTC (corresponding with 01:30 US Eastern Standard Time at UTC offset -05:00).

2:30 AM on 12 March 2017 in America/New_York does not exist, so GetNamedTimeZoneEpochNanoseconds~~( "America/New_York", 2017, 3, 12, 2, 30, 0, 0, 0, 0)~~ for that time zone and ISO date-time would return an empty List.

# 14.6.4 SystemTimeZoneIdentifier ( )

The implementation-defined abstract operation SystemTimeZoneIdentifier takes no arguments and returns an available time zone identifier. It returns a String representing the host environment's current time zone, which is either ~~a String representing a UTC offset for whichIsTimeZoneOffsetString returns true, or a primary time zone identifier~~a primary time zone identifier or an offset time zone identifier. It performs the following steps when called:

1. If the implementation only supports the UTC time zone, return "UTC".
2. Let systemTimeZoneString be the String representing the host environment's current time zone as a time zone identifier in normalized format, either a primary time zone identifier or an offset time zone identifier.
3. Return systemTimeZoneString.

Note

To ensure the level of functionality that implementations commonly provide in the methods of the Date object, it is recommended that SystemTimeZoneIdentifier return an IANA time zone name corresponding to the host environment's time zone setting, if such a thing exists. GetNamedTimeZoneEpochNanoseconds and GetNamedTimeZoneOffsetNanoseconds must reflect the local political rules for standard time and daylight saving time in that time zone, if such rules exist.

For example, if the host environment is a browser on a system where the user has chosen US Eastern Time as their time zone, SystemTimeZoneIdentifier returns "America/New_York".

# 14.6.5 Time Zone Offset String ~~Format~~ Formats

~~

ECMAScript defines a string interchange format for UTC offsets, derived from ISO 8601. The format is described by the following grammar.

~~

ECMAScript defines string interchange formats for UTC offsets, derived from ISO 8601. UTC offsets that represent offset time zone identifiers, or that are intended for interoperability with ISO 8601, use only hours and minutes and are specified by UTCOffset[~SubMinutePrecision]. UTC offsets that represent the offset of a named time zone can be more precise, and are specified by UTCOffset[+SubMinutePrecision].

These formats are described by the ISO String grammar in 13.31.

[...]

Editor's Note

The grammar in this section should be deleted; it is replaced by the ISO 8601 / RFC 9557 string grammar in 13.31.

# 14.6.6 LocalTime ( t )

The abstract operation LocalTime takes argument t (a finite time value) and returns an integral Number. It converts t from UTC to local time. The local political rules for standard time and daylight saving time in effect at t should be used to determine the result in the way specified in this section. It performs the following steps when called:

1. Let systemTimeZoneIdentifier be SystemTimeZoneIdentifier().
2. Let parseResult be ! ParseTimeZoneIdentifier(systemTimeZoneIdentifier).
3. If ~~IsTimeZoneOffsetString(systemTimeZoneIdentifier) is true~~parseResult.[[OffsetMinutes]] is not empty, then
   1. Let offsetNs be ~~ParseTimeZoneOffsetString(systemTimeZoneIdentifier)~~parseResult.[[OffsetMinutes]] × (60 × 109).
4. Else,
   1. Let offsetNs be GetNamedTimeZoneOffsetNanoseconds(systemTimeZoneIdentifier, ℤ(ℝ(t) × 106)).
5. Let offsetMs be truncate(offsetNs / 106).
6. Return t \+ 𝔽(offsetMs).

Note 1

If political rules for the local time t are not available within the implementation, the result is t because SystemTimeZoneIdentifier returns "UTC" and GetNamedTimeZoneOffsetNanoseconds returns 0.

Note 2

It is required for time zone aware implementations (and recommended for all others) to use the time zone information of the IANA Time Zone Database https://www.iana.org/time-zones/.

Note 3

Two different input time values tUTC are converted to the same local time tlocal at a negative time zone transition when there are repeated times (e.g. the daylight saving time ends or the time zone adjustment is decreased.).

LocalTime(UTC(tlocal)) is not necessarily always equal to tlocal. Correspondingly, UTC(LocalTime(tUTC)) is not necessarily always equal to tUTC.

# 14.6.7 UTC ( t )

The abstract operation UTC takes argument t (a Number) and returns a time value. It converts t from local time to a UTC time value. The local political rules for standard time and daylight saving time in effect at t should be used to determine the result in the way specified in this section. It performs the following steps when called:

1. If t is not finite, return NaN.
2. Let systemTimeZoneIdentifier be SystemTimeZoneIdentifier().
3. Let parseResult be ! ParseTimeZoneIdentifier(systemTimeZoneIdentifier).
4. If ~~IsTimeZoneOffsetString(systemTimeZoneIdentifier) is true~~parseResult.[[OffsetMinutes]] is not empty, then
   1. Let offsetNs be ~~ParseTimeZoneOffsetString(systemTimeZoneIdentifier)~~parseResult.[[OffsetMinutes]] × (60 × 109).
5. Else,
   1. Let isoDateTime be TimeValueToISODateTimeRecord(t).
   2. Let possibleInstants be GetNamedTimeZoneEpochNanoseconds(systemTimeZoneIdentifier, ~~ℝ(YearFromTime(t)), ℝ(MonthFromTime(t)) + 1, ℝ(DateFromTime(t)), ℝ(HourFromTime(t)), ℝ(MinFromTime(t)), ℝ(SecFromTime(t)), ℝ(msFromTime(t)), 0, 0~~isoDateTime).
   3. NOTE: The following steps ensure that when t represents local time repeating multiple times at a negative time zone transition (e.g. when the daylight saving time ends or the time zone offset is decreased due to a time zone rule change) or skipped local time at a positive time zone transition (e.g. when the daylight saving time starts or the time zone offset is increased due to a time zone rule change), t is interpreted using the time zone offset before the transition.
   4. If possibleInstants is not empty, then
      1. Let disambiguatedInstant be possibleInstants[0].
   5. Else,
      1. NOTE: t represents a local time skipped at a positive time zone transition (e.g. due to daylight saving time starting or a time zone rule change increasing the UTC offset).
      2. Let possibleInstantsBefore be GetNamedTimeZoneEpochNanoseconds(systemTimeZoneIdentifier, ~~ℝ(YearFromTime(tBefore)), ℝ(MonthFromTime(tBefore)) + 1, ℝ(DateFromTime(tBefore)), ℝ(HourFromTime(tBefore)), ℝ(MinFromTime(tBefore)), ℝ(SecFromTime(tBefore)), ℝ(msFromTime(tBefore)), 0, 0~~TimeValueToISODateTimeRecord(tBefore)), where tBefore is the largest integral Number < t for which possibleInstantsBefore is not empty (i.e., tBefore represents the last local time before the transition).
      3. Let disambiguatedInstant be the last element of possibleInstantsBefore.
   6. Let offsetNs be GetNamedTimeZoneOffsetNanoseconds(systemTimeZoneIdentifier, disambiguatedInstant).
6. Let offsetMs be truncate(offsetNs / 106).
7. Return t \- 𝔽(offsetMs).

Input t is nominally a time value but may be any Number value. The algorithm must not limit t to the time value range, so that inputs corresponding with a boundary of the time value range can be supported regardless of local UTC offset. For example, the maximum time value is 8.64 × 1015, corresponding with "+275760-09-13T00:00:00Z". In an environment where the local time zone offset is ahead of UTC by 1 hour at that instant, it is represented by the larger input of 8.64 × 1015 \+ 3.6 × 106, corresponding with "+275760-09-13T01:00:00+01:00".

If political rules for the local time t are not available within the implementation, the result is t because SystemTimeZoneIdentifier returns "UTC" and GetNamedTimeZoneOffsetNanoseconds returns 0.

Note 1

It is required for time zone aware implementations (and recommended for all others) to use the time zone information of the IANA Time Zone Database https://www.iana.org/time-zones/.

1:30 AM on 5 November 2017 in America/New_York is repeated twice (fall backward), but it must be interpreted as 1:30 AM UTC-04 instead of 1:30 AM UTC-05. In UTC(TimeClip(MakeDate(MakeDay(2017, 10, 5), MakeTime(1, 30, 0, 0)))), the value of offsetMs is -4 × msPerHour.

2:30 AM on 12 March 2017 in America/New_York does not exist, but it must be interpreted as 2:30 AM UTC-05 (equivalent to 3:30 AM UTC-04). In UTC(TimeClip(MakeDate(MakeDay(2017, 2, 12), MakeTime(2, 30, 0, 0)))), the value of offsetMs is -5 × msPerHour.

Note 2

UTC(LocalTime(tUTC)) is not necessarily always equal to tUTC. Correspondingly, LocalTime(UTC(tlocal)) is not necessarily always equal to tlocal.

[...]

# 14.6.8 TimeString ( tv )

The abstract operation TimeString takes argument tv (a Number, but not NaN) and returns a String. It performs the following steps when called:

1. ~~Let hour be ToZeroPaddedDecimalString(ℝ(HourFromTime(tv)), 2).~~
2. ~~Let minute be ToZeroPaddedDecimalString(ℝ(MinFromTime(tv)), 2).~~
3. ~~Let second be ToZeroPaddedDecimalString(ℝ(SecFromTime(tv)), 2).~~
4. Let timeString be FormatTimeString(ℝ(HourFromTime(tv)), ℝ(MinFromTime(tv)), ℝ(SecFromTime(tv)), 0, 0).
5. Return the string-concatenation of ~~hour , ":", minute, ":", second~~timeString, the code unit 0x0020 (SPACE), and "GMT".

[...]

# 14.6.9 TimeZoneString ( tv )

The abstract operation TimeZoneString takes argument tv (an integral Number) and returns a String. It performs the following steps when called:

1. Let systemTimeZoneIdentifier be SystemTimeZoneIdentifier().
2. ~~IfIsTimeZoneOffsetString(systemTimeZoneIdentifier) is true, then~~
   1. ~~Let offsetNs be ParseTimeZoneOffsetString(systemTimeZoneIdentifier).~~
3. ~~Else,~~
4. Let offsetMinutes be ! ParseTimeZoneIdentifier(systemTimeZoneIdentifier).[[OffsetMinutes]].
5. If offsetMinutes is empty, then
   1. Let offsetNs be GetNamedTimeZoneOffsetNanoseconds(systemTimeZoneIdentifier, ℤ(ℝ(tv) × 106)).
   2. Set offsetMinutes to truncate(offsetNs / (60 × 109)).
6. ~~Let offset be 𝔽(truncate(offsetNs / 106)).~~
7. ~~If offset is +0𝔽 or offset > +0𝔽, then~~
   1. ~~Let offsetSign be "+".~~
   2. ~~Let absOffset be offset.~~
8. ~~Else,~~
   1. ~~Let offsetSign be "-".~~
   2. ~~Let absOffset be -offset.~~
9. ~~Let offsetMin be ToZeroPaddedDecimalString(ℝ(MinFromTime(absOffset)), 2).~~
10. ~~Let offsetHour be ToZeroPaddedDecimalString(ℝ(HourFromTime(absOffset)), 2).~~
11. Let offsetString be FormatOffsetTimeZoneIdentifier(offsetMinutes, unseparated).
12. Let tzName be an implementation-defined string that is either the empty String or the string-concatenation of the code unit 0x0020 (SPACE), the code unit 0x0028 (LEFT PARENTHESIS), an implementation-defined timezone name, and the code unit 0x0029 (RIGHT PARENTHESIS).
13. ~~Return thestring-concatenation of offsetSign, offsetHour, offsetMin, and tzName.~~
14. Return the string-concatenation of offsetString and tzName.

~~

# IsTimeZoneOffsetString ( offsetString: a String, ): a Boolean

~~

# 14.6.10 IsOffsetTimeZoneIdentifier ( offsetString )

The abstract operation IsOffsetTimeZoneIdentifier takes argument offsetString (a String) and returns a Boolean. The return value indicates whether offsetString conforms to the grammar given by ~~UTCOffset~~UTCOffset[~SubMinutePrecision]. It performs the following steps when called:

1. Let parseResult be ParseText(StringToCodePoints(offsetString), ~~UTCOffset~~UTCOffset[~SubMinutePrecision]).
2. If parseResult is a List of errors, return false.
3. Return true.

~~

# ParseTimeZoneOffsetString ( offsetString: a String, ): an integer

~~

# 14.6.11 ParseDateTimeUTCOffset ( offsetString )

The abstract operation ParseDateTimeUTCOffset takes argument offsetString (a String) and returns either a normal completion containing an integer in the interval from -86400 × 109 (exclusive) to 86400 × 109 (exclusive), or a throw completion. The return value is the UTC offset, as a number of nanoseconds, that corresponds to the String offsetString. If offsetString is invalid, a RangeError is thrown. It performs the following steps when called:

1. Let parseResult be ParseText(StringToCodePoints(offsetString), ~~UTCOffset~~UTCOffset[+SubMinutePrecision]).
2. ~~Assert : parseResult is not a List of errors.~~
3. If parseResult is a List of errors, throw a RangeError exception.
4. Assert: parseResult contains a ASCIISign Parse Node.
5. Let parsedSign be the source text matched by the ASCIISign Parse Node contained within parseResult.
6. If parsedSign is the single code point U+002D (HYPHEN-MINUS), then
   1. Let sign be -1.
7. Else,
   1. Let sign be 1.
8. NOTE: Applications of StringToNumber below do not lose precision, since each of the parsed values is guaranteed to be a sufficiently short string of decimal digits.
9. Assert: parseResult contains an Hour Parse Node.
10. Let parsedHours be the source text matched by the Hour Parse Node contained within parseResult.
11. Let hours be ℝ(StringToNumber(CodePointsToString(parsedHours))).
12. If parseResult does not contain a MinuteSecond Parse Node, then
    1. Let minutes be 0.
13. Else,
    1. Let parsedMinutes be the source text matched by the first MinuteSecond Parse Node contained within parseResult.
    2. Let minutes be ℝ(StringToNumber(CodePointsToString(parsedMinutes))).
14. If parseResult does not contain two MinuteSecond Parse Nodes, then
    1. Let seconds be 0.
15. Else,
    1. Let parsedSeconds be the source text matched by the second MinuteSecond Parse Node contained within parseResult.
    2. Let seconds be ℝ(StringToNumber(CodePointsToString(parsedSeconds))).
16. If parseResult does not contain a TemporalDecimalFraction Parse Node, then
    1. Let nanoseconds be 0.
17. Else,
    1. Let parsedFraction be the source text matched by the TemporalDecimalFraction Parse Node contained within parseResult.
    2. Let fraction be the string-concatenation of CodePointsToString(parsedFraction) and "000000000".
    3. Let nanosecondsString be the substring of fraction from 1 to 10.
    4. Let nanoseconds be ℝ(StringToNumber(nanosecondsString)).
18. Return sign × (((hours × 60 + minutes) × 60 + seconds) × 109 \+ nanoseconds).

# 14.7 The Date Constructor

# 14.7.1 Date ( ...values )

This function performs the following steps when called:

1. ~~If NewTarget is undefined, then~~
   1. ~~Let now be the time value (UTC) identifying the current time.~~
   2. ~~ReturnToDateString(now).~~
2. If NewTarget is undefined, return ToDateString(SystemUTCEpochMilliseconds()).
3. Let numberOfArgs be the number of elements in values.
4. If numberOfArgs = 0, then
   1. Let dv be ~~thetime value (UTC) identifying the current time~~ SystemUTCEpochMilliseconds().
5. Else if numberOfArgs = 1, then
   1. Let value be values[0].
   2. If value is an Object and value has a [[DateValue]] internal slot, then
      1. Let tv be value.[[DateValue]].
   3. Else,
      1. Let v be ? ToPrimitive(value).
      2. If v is a String, then
         1. Assert: The next step never returns an abrupt completion because v is a String.
         2. Let tv be the result of parsing v as a date, in exactly the same manner as for the `parse` method (21.4.3.2).
      3. Else,
         1. Let tv be ? ToNumber(v).
   4. Let dv be TimeClip(tv).
6. Else,
   1. Assert: numberOfArgs ≥ 2.
   2. Let y be ? ToNumber(values[0]).
   3. Let m be ? ToNumber(values[1]).
   4. If numberOfArgs > 2, let dt be ? ToNumber(values[2]); else let dt be 1𝔽.
   5. If numberOfArgs > 3, let h be ? ToNumber(values[3]); else let h be +0𝔽.
   6. If numberOfArgs > 4, let min be ? ToNumber(values[4]); else let min be +0𝔽.
   7. If numberOfArgs > 5, let s be ? ToNumber(values[5]); else let s be +0𝔽.
   8. If numberOfArgs > 6, let milli be ? ToNumber(values[6]); else let milli be +0𝔽.
   9. Let yr be MakeFullYear(y).
   10. Let finalDate be MakeDate(MakeDay(yr, m, dt), MakeTime(h, min, s, milli)).
   11. Let dv be TimeClip(UTC(finalDate)).
7. Let O be ? OrdinaryCreateFromConstructor(NewTarget, "%Date.prototype%", « [[DateValue]] »).
8. Set O.[[DateValue]] to dv.
9. Return O.

# 14.8 Properties of the Date Constructor

# 14.8.1 Date.now ( )

~~

This function returns the time value designating the UTC date and time of the occurrence of the call to it.

~~

This function performs the following steps when called:

1. Return SystemUTCEpochMilliseconds().

# 14.9 Properties of the Date Prototype Object

# 14.9.1 Date.prototype.toTemporalInstant ( )

This method performs the following steps when called:

1. Let dateObject be the this value.
2. Perform ? RequireInternalSlot(dateObject, [[DateValue]]).
3. Let t be dateObject.[[DateValue]].
4. Let ns be ? NumberToBigInt(t) × ℤ(106).
5. Return ! CreateTemporalInstant(ns).

# 15 Amendments to the ECMAScript® 2023 Internationalization API Specification

Editor's Note

This section lists amendments which must be made to ECMA-402, the ECMAScript® 2023 Internationalization API Specification. Text to be added is marked like this, and text to be deleted is marked ~~like this~~. Blocks of unmodified text between modified sections are marked by [...].

This text is based on top of the ECMA-402 spec text from commit 537afda7be28a443b79b3fd7e6c836a16ce4f75f.

# 15.1 Case Sensitivity and Case Mapping

Editor's Note

These definitions are moved into ECMA-262.

The Strings used to identify locales, currencies, scripts, and time zones are interpreted in an ASCII-case-insensitive manner, treating the code units 0x0041 through 0x005A (corresponding to Unicode characters LATIN CAPITAL LETTER A through LATIN CAPITAL LETTER Z) as equivalent to the corresponding code units 0x0061 through 0x007A (corresponding to Unicode characters LATIN SMALL LETTER A through LATIN SMALL LETTER Z), both inclusive. No other case folding equivalences are applied.

Note

For example, "ß" (U+00DF) must not match or be mapped to "SS" (U+0053, U+0053). "ı" (U+0131) must not match or be mapped to "I" (U+0049).

~~

The _ASCII-uppercase_ of a String S is the String derived from S by replacing each occurrence of an ASCII lowercase letter code unit (0x0061 through 0x007A, inclusive) with the corresponding ASCII uppercase letter code unit (0x0041 through 0x005A, inclusive) while preserving all other code units.

The _ASCII-lowercase_ of a String S is the String derived from S by replacing each occurrence of an ASCII uppercase letter code unit (0x0041 through 0x005A, inclusive) with the corresponding ASCII lowercase letter code unit (0x0061 through 0x007A, inclusive) while preserving all other code units.

A String A is an _ASCII-case-insensitive match_ for String B if the ASCII-uppercase of A is exactly the same sequence of code units as the ASCII-uppercase of B. A sequence of Unicode code points A is an ASCII-case-insensitive match for B if B is an ASCII-case-insensitive match for ! CodePointsToString(A).

~~ ~~

# 15.2 Calendar Types

This specification identifies calendars using a calendar type as defined by Unicode Technical Standard #35 Part 4 Dates, Section 2 Calendar Elements. Their canonical form is a string containing only Unicode Basic Latin lowercase letters (U+0061 LATIN SMALL LETTER A through U+007A LATIN SMALL LETTER Z) with zero or more medial hyphens (U+002D HYPHEN-MINUS).

# 15.2.1 AvailableCalendars ( )

The abstract operation AvailableCalendars takes no arguments and returns a List of Strings. The returned List is sorted according to lexicographic code unit order, and contains unique calendar types in canonical form (12.1) identifying the calendars for which the implementation provides the functionality of Intl.DateTimeFormat objects, including their aliases (e.g., either both or neither of "islamicc" and "islamic-civil"). The List must include "iso8601".

~~

# 15.3 Abstract Operations

Editor's Note

In this section, some abstract operations that manipulate options objects are to be moved from ECMA-402 into ECMA-262.

[...]

~~

# 15.3.1 GetOption ( options, property, type, values, default )

The abstract operation GetOption takes arguments options (an Object), property (a property key), type (boolean or string), values (empty or a List of ECMAScript language values), and default (required or an ECMAScript language value) and returns either a normal completion containing an ECMAScript language value or a throw completion. It extracts the value of the specified property of options, converts it to the required type, checks whether it is allowed by values if values is not empty, and substitutes default if the value is undefined. It performs the following steps when called:

1. Let value be ? Get(options, property).
2. If value is undefined, then
   1. If default is required, throw a RangeError exception.
   2. Return default.
3. If type is boolean, then
   1. Set value to ToBoolean(value).
4. Else,
   1. Assert: type is string.
   2. Set value to ? ToString(value).
5. If values is not empty and values does not contain value, throw a RangeError exception.
6. Return value.

~~

[...]

| ~~ Table 29: Rounding modes in Intl.NumberFormat Identifier | Description                                     | Examples: Round to 0 fraction digits |
| ----------------------------------------------------------- | ----------------------------------------------- | ------------------------------------ |
| -1.5                                                        | 0.4                                             | 0.5                                  |
| "ceil"                                                      | Toward positive infinity                        | ⬆️ [-1]                              |
| "floor"                                                     | Toward negative infinity                        | ⬇️ [-2]                              |
| "expand"                                                    | Away from zero                                  | ⬇️ [-2]                              |
| "trunc"                                                     | Toward zero                                     | ⬆️ [-1]                              |
| "halfCeil"                                                  | Ties toward positive infinity                   | ⬆️ [-1]                              |
| "halfFloor"                                                 | Ties toward negative infinity                   | ⬇️ [-2]                              |
| "halfExpand"                                                | Ties away from zero                             | ⬇️ [-2]                              |
| "halfTrunc"                                                 | Ties toward zero                                | ⬆️ [-1]                              |
| "halfEven"                                                  | Ties toward an even rounding increment multiple | ⬇️ [-2]                              |
| Note                                                        |                                                 |                                      |

The examples are illustrative of the unique behaviour of each option. ⬆️ means "resolves toward positive infinity"; ⬇️ means "resolves toward negative infinity".

~~ ~~

# 15.3.2 GetUnsignedRoundingMode ( roundingMode, sign )

The abstract operation GetUnsignedRoundingMode takes arguments roundingMode (a String) and sign (negative or positive) and returns a specification type from the Unsigned Rounding Mode column of Table 30. It returns the rounding mode that should be applied to the absolute value of a number to produce the same result as if roundingMode, one of the String values in the Identifier column of Table 29, were applied to the signed value of the number (negative if sign is negative, or positive otherwise). It performs the following steps when called:

1. Return the specification type in the Unsigned Rounding Mode column of Table 30 for the row where the value in the Identifier column is roundingMode and the value in the Sign column is sign.

| Table 30: Conversion from rounding mode to unsigned rounding mode Identifier | Sign          | Unsigned Rounding Mode |
| ---------------------------------------------------------------------------- | ------------- | ---------------------- |
| "ceil"                                                                       | positive      | infinity               |
| negative                                                                     | zero          |                        |
| "floor"                                                                      | positive      | zero                   |
| negative                                                                     | infinity      |                        |
| "expand"                                                                     | positive      | infinity               |
| negative                                                                     | infinity      |                        |
| "trunc"                                                                      | positive      | zero                   |
| negative                                                                     | zero          |                        |
| "halfCeil"                                                                   | positive      | half-infinity          |
| negative                                                                     | half-zero     |                        |
| "halfFloor"                                                                  | positive      | half-zero              |
| negative                                                                     | half-infinity |                        |
| "halfExpand"                                                                 | positive      | half-infinity          |
| negative                                                                     | half-infinity |                        |
| "halfTrunc"                                                                  | positive      | half-zero              |
| negative                                                                     | half-zero     |                        |
| "halfEven"                                                                   | positive      | half-even              |
| negative                                                                     | half-even     |                        |
| ~~ ~~                                                                        |               |                        |

# 15.3.3 ApplyUnsignedRoundingMode ( x, r1, r2, unsignedRoundingMode )

The abstract operation ApplyUnsignedRoundingMode takes arguments x (a mathematical value), r1 (a mathematical value), r2 (a mathematical value), and unsignedRoundingMode (a specification type from the Unsigned Rounding Mode column of Table 30, or undefined) and returns a mathematical value. It considers x, bracketed below by r1 and above by r2, and returns either r1 or r2 according to unsignedRoundingMode. It performs the following steps when called:

1. If x is equal to r1, return r1.
2. Assert: r1 < x < r2.
3. Assert: unsignedRoundingMode is not undefined.
4. If unsignedRoundingMode is zero, return r1.
5. If unsignedRoundingMode is infinity, return r2.
6. Let d1 be x – r1.
7. Let d2 be r2 – x.
8. If d1 < d2, return r1.
9. If d2 < d1, return r2.
10. Assert: d1 is equal to d2.
11. If unsignedRoundingMode is half-zero, return r1.
12. If unsignedRoundingMode is half-infinity, return r2.
13. Assert: unsignedRoundingMode is half-even.
14. Let cardinality be (r1 / (r2 – r1)) modulo 2.
15. If cardinality is 0, return r1.
16. Return r2.

~~

# 15.4 The Intl.DateTimeFormat Constructor

[...]

# 15.4.1 CreateDateTimeFormat ( newTarget, locales, options, required, defaults [ , toLocaleStringTimeZone ] )

The abstract operation CreateDateTimeFormat takes arguments newTarget (a constructor), locales (an ECMAScript language value), options (an ECMAScript language value), required (date, time, or any), and defaults (date, time, or all) and optional argument toLocaleStringTimeZone (a primary time zone identifier) and returns either a normal completion containing a DateTimeFormat object or a throw completion.

If the additional toLocaleStringTimeZone argument is provided, the time zone will be overridden and some adjustments will be made to the defaults in order to implement the behaviour of `Temporal.ZonedDateTime.prototype.toLocaleString`.

It performs the following steps when called:

1. Let dateTimeFormat be ? OrdinaryCreateFromConstructor(newTarget, "%Intl.DateTimeFormat.prototype%", « [[InitializedDateTimeFormat]], [[Locale]], [[Calendar]], [[NumberingSystem]], [[TimeZone]], [[HourCycle]], [[DateStyle]], [[TimeStyle]], [[DateTimeFormat]], [[TemporalPlainDateFormat]], [[TemporalPlainYearMonthFormat]], [[TemporalPlainMonthDayFormat]], [[TemporalPlainTimeFormat]], [[TemporalPlainDateTimeFormat]], [[TemporalInstantFormat]], [[BoundFormat]] »).
2. Let requestedLocales be ? CanonicalizeLocaleList(locales).
3. Set options to ? CoerceOptionsToObject(options).
4. Let opt be a new Record.
5. Let matcher be ? GetOption(options, "localeMatcher", string, « "lookup", "best fit" », "best fit").
6. Set opt.[[localeMatcher]] to matcher.
7. Let calendar be ? GetOption(options, "calendar", string, empty, undefined).
8. If calendar is not undefined, then
   1. If calendar cannot be matched by the `type` Unicode locale nonterminal, throw a RangeError exception.
9. Set opt.[[ca]] to calendar.
10. Let numberingSystem be ? GetOption(options, "numberingSystem", string, empty, undefined).
11. If numberingSystem is not undefined, then
    1. If numberingSystem cannot be matched by the `type` Unicode locale nonterminal, throw a RangeError exception.
12. Set opt.[[nu]] to numberingSystem.
13. Let hour12 be ? GetOption(options, "hour12", boolean, empty, undefined).
14. Let hourCycle be ? GetOption(options, "hourCycle", string, « "h11", "h12", "h23", "h24" », undefined).
15. If hour12 is not undefined, then
    1. Set hourCycle to null.
16. Set opt.[[hc]] to hourCycle.
17. Let r be ResolveLocale(%Intl.DateTimeFormat%.[[AvailableLocales]], requestedLocales, opt, %Intl.DateTimeFormat%.[[RelevantExtensionKeys]], %Intl.DateTimeFormat%.[[LocaleData]]).
18. Set dateTimeFormat.[[Locale]] to r.[[Locale]].
19. Let resolvedCalendar be r.[[ca]].
20. Set dateTimeFormat.[[Calendar]] to resolvedCalendar.
21. Set dateTimeFormat.[[NumberingSystem]] to r.[[nu]].
22. Let resolvedLocaleData be r.[[LocaleData]].
23. If hour12 is true, then
    1. Let hc be resolvedLocaleData.[[hourCycle12]].
24. Else if hour12 is false, then
    1. Let hc be resolvedLocaleData.[[hourCycle24]].
25. Else,
    1. Assert: hour12 is undefined.
    2. Let hc be r.[[hc]].
    3. If hc is null, set hc to resolvedLocaleData.[[hourCycle]].
26. Set dateTimeFormat.[[HourCycle]] to hc.
27. Let timeZone be ? Get(options, "timeZone").
28. If timeZone is undefined, then
    1. If toLocaleStringTimeZone is present, then
    1. Set timeZone to toLocaleStringTimeZone.
       2. Else,
    1. Set timeZone to SystemTimeZoneIdentifier().
29. Else,
    1. If toLocaleStringTimeZone is present, throw a TypeError exception.
    2. Set timeZone to ? ToString(timeZone).
30. If IsTimeZoneOffsetString(timeZone) is true, then
    1. Let parseResult be ParseText(StringToCodePoints(timeZone), ~~UTCOffset~~UTCOffset[~SubMinutePrecision]).
    2. Assert: parseResult is a Parse Node.
    3. ~~If parseResult contains more than one MinuteSecond Parse Node, throw a RangeError exception.~~
    4. Let offsetNanoseconds be ~~ParseTimeZoneOffsetString~~? ParseDateTimeUTCOffset(timeZone).
    5. Let offsetMinutes be offsetNanoseconds / (6 × 1010).
    6. Set timeZone to FormatOffsetTimeZoneIdentifier(offsetMinutes).
31. ~~Else if IsValidTimeZoneName( timeZone) is true, then~~
32. Else,
    1. Let timeZoneIdentifierRecord be GetAvailableNamedTimeZoneIdentifier(timeZone).
    2. If timeZoneIdentifierRecord is empty, then
    3. Throw a RangeError exception.
       3. Set timeZone to ~~CanonicalizeTimeZoneName( timeZone)~~timeZoneIdentifierRecord.[[Identifier]].
33. ~~Else,~~
    1. ~~Throw a RangeError exception.~~
34. Set dateTimeFormat.[[TimeZone]] to timeZone.
35. Let formatOptions be a new Record.
36. Set formatOptions.[[hourCycle]] to hc.
37. Let hasExplicitFormatComponents be false.
38. For each row of Table 16, except the header row, in table order, do
    1. Let prop be the name given in the Property column of the row.
    2. If prop is "fractionalSecondDigits", then
    3. Let value be ? GetNumberOption(options, "fractionalSecondDigits", 1, 3, undefined).
       3. Else,
    4. Let values be a List whose elements are the strings given in the Values column of the row.
    5. Let value be ? GetOption(options, prop, string, values, undefined).
       4. Set formatOptions.[[<prop>]] to value.
       5. If value is not undefined, then
    6. Set hasExplicitFormatComponents to true.
39. Let formatMatcher be ? GetOption(options, "formatMatcher", string, « "basic", "best fit" », "best fit").
40. Let dateStyle be ? GetOption(options, "dateStyle", string, « "full", "long", "medium", "short" », undefined).
41. Set dateTimeFormat.[[DateStyle]] to dateStyle.
42. Let timeStyle be ? GetOption(options, "timeStyle", string, « "full", "long", "medium", "short" », undefined).
43. Set dateTimeFormat.[[TimeStyle]] to timeStyle.
44. Let formats be resolvedLocaleData.[[formats]].[[<resolvedCalendar>]].
45. If dateStyle is not undefined or timeStyle is not undefined, then
    1. If hasExplicitFormatComponents is true, then
    1. Throw a TypeError exception.
       2. If required is date and timeStyle is not undefined, then
    1. Throw a TypeError exception.
       3. If required is time and dateStyle is not undefined, then
    1. Throw a TypeError exception.
       4. Let styles be resolvedLocaleData.[[styles]].[[<resolvedCalendar>]].
       5. Let bestFormat be DateTimeStyleFormat(dateStyle, timeStyle, styles).
       6. If dateStyle is not undefined, then
    1. Set dateTimeFormat.[[TemporalPlainDateFormat]] to AdjustDateTimeStyleFormat(formats, bestFormat, formatMatcher, « "weekday", "era", "year", "month", "day" »).
    1. Set dateTimeFormat.[[TemporalPlainYearMonthFormat]] to AdjustDateTimeStyleFormat(formats, bestFormat, formatMatcher, « "era", "year", "month" »).
    1. Set dateTimeFormat.[[TemporalPlainMonthDayFormat]] to AdjustDateTimeStyleFormat(formats, bestFormat, formatMatcher, « "month", "day" »).
       7. Else,
    1. Set dateTimeFormat.[[TemporalPlainDateFormat]] to null.
    1. Set dateTimeFormat.[[TemporalPlainYearMonthFormat]] to null.
    1. Set dateTimeFormat.[[TemporalPlainMonthDayFormat]] to null.
       8. If timeStyle is not undefined, then
    1. Set dateTimeFormat.[[TemporalPlainTimeFormat]] to AdjustDateTimeStyleFormat(formats, bestFormat, formatMatcher, « "dayPeriod", "hour", "minute", "second", "fractionalSecondDigits" »).
       9. Else,
    1. Set dateTimeFormat.[[TemporalPlainTimeFormat]] to null.
       10. Set dateTimeFormat.[[TemporalPlainDateTimeFormat]] to AdjustDateTimeStyleFormat(formats, bestFormat, formatMatcher, « "weekday", "era", "year", "month", "day", "dayPeriod", "hour", "minute", "second", "fractionalSecondDigits" »).
       11. Set dateTimeFormat.[[TemporalInstantFormat]] to bestFormat.
46. Else,
    1. ~~Let needDefaults be true.~~
    2. ~~If required is date or any, then~~
    3. ~~For eachproperty name prop of « "weekday", "year", "month", "day" », do~~
       1. ~~Let value be formatOptions.[[<prop>]].~~
       2. ~~If value is not undefined, set needDefaults to false.~~
          3. ~~If required is time or any, then~~
    4. ~~For eachproperty name prop of « "dayPeriod", "hour", "minute", "second", "fractionalSecondDigits" », do~~
       1. ~~Let value be formatOptions.[[<prop>]].~~
       2. ~~If value is not undefined, set needDefaults to false.~~
          4. ~~If needDefaults is true and defaults is either date or all, then~~
    5. ~~For eachproperty name prop of « "year", "month", "day" », do~~
       1. ~~Set formatOptions.[[<prop>]] to "numeric".~~
          5. ~~If needDefaults is true and defaults is either time or all, then~~
    6. ~~For eachproperty name prop of « "hour", "minute", "second" », do~~
       1. ~~Set formatOptions.[[<prop>]] to "numeric".~~
          6. ~~Let formats be resolvedLocaleData.[[formats]].[[<resolvedCalendar>]].~~
          7. ~~If formatMatcher is "basic", then~~
    7. ~~Let bestFormat be BasicFormatMatcher(formatOptions, formats).~~
       8. ~~Else,~~
    8. ~~Let bestFormat be BestFitFormatMatcher(formatOptions, formats).~~
       9. Let bestFormat be GetDateTimeFormat(formats, formatMatcher, formatOptions, required, defaults, all).
       10. Set dateTimeFormat.[[TemporalPlainDateFormat]] to GetDateTimeFormat(formats, formatMatcher, formatOptions, date, date, relevant).
       11. Set dateTimeFormat.[[TemporalPlainYearMonthFormat]] to GetDateTimeFormat(formats, formatMatcher, formatOptions, year-month, year-month, relevant).
       12. Set dateTimeFormat.[[TemporalPlainMonthDayFormat]] to GetDateTimeFormat(formats, formatMatcher, formatOptions, month-day, month-day, relevant).
       13. Set dateTimeFormat.[[TemporalPlainTimeFormat]] to GetDateTimeFormat(formats, formatMatcher, formatOptions, time, time, relevant).
       14. Set dateTimeFormat.[[TemporalPlainDateTimeFormat]] to GetDateTimeFormat(formats, formatMatcher, formatOptions, any, all, relevant).
       15. If toLocaleStringTimeZone is present, then
    9. Set dateTimeFormat.[[TemporalInstantFormat]] to GetDateTimeFormat(formats, formatMatcher, formatOptions, any, zoned-date-time, all).
       16. Else,
    10. Set dateTimeFormat.[[TemporalInstantFormat]] to GetDateTimeFormat(formats, formatMatcher, formatOptions, any, all, all).
47. Set dateTimeFormat.[[DateTimeFormat]] to bestFormat.
48. ~~If bestFormat has a field [[hour]], then~~
    1. ~~Set dateTimeFormat.[[HourCycle]] to hc.~~
49. Return dateTimeFormat.

# 15.5 Properties of the Intl.DateTimeFormat Constructor

[...]

# 15.5.1 Internal slots

[...]

The value of the [[LocaleData]] internal slot is implementation-defined within the constraints described in 9.1 and the following additional constraints, for all locale values locale:

- [[LocaleData]].[[<locale>]].[[nu]] must be a List that does not include the values "native", "traditio", or "finance".
- [[LocaleData]].[[<locale>]].[[hc]] must be « null, "h11", "h12", "h23", "h24" ».
- [[LocaleData]].[[<locale>]].[[hourCycle]] must be one of the String values "h11", "h12", "h23", or "h24".
- [[LocaleData]].[[<locale>]].[[hourCycle12]] must be one of the String values "h11" or "h12".
- [[LocaleData]].[[<locale>]].[[hourCycle24]] must be one of the String values "h23" or "h24".
- [[LocaleData]].[[<locale>]] must have a [[formats]] field. The value of this [[formats]] field must be a Record with a [[<calendar>]] field for each calendar value calendar. The value of each [[<calendar>]] field must be a List of DateTime Format Records. Multiple Records in such a List may use the same subset of the fields as long as the corresponding values differ for at least one field. The following subsets must be available for each locale:
  - weekday, year, month, day, hour, minute, second, fractionalSecondDigits
  - weekday, year, month, day, hour, minute, second
  - weekday, year, month, day
  - year, month, day
  - year, month
  - month, day
  - month
  - hour, minute, second, fractionalSecondDigits
  - hour, minute, second
  - hour, minute
  - dayPeriod, hour
  - dayPeriod, hour, minute, second
  - dayPeriod, hour, minute
- [[LocaleData]].[[<locale>]] must have a [[styles]] field. The value of this [[styles]] field must be a Record with a [[<calendar>]] field for each calendar value calendar. The value of each [[<calendar>]] field must be a DateTime Styles Record.

# 15.6 Abstract Operations for DateTimeFormat Objects

# 15.6.1 GetDateTimeFormat ( formats, matcher, options, required, defaults, inherit )

The abstract operation GetDateTimeFormat takes arguments formats (a List of DateTime Format Records), matcher (either "basic" or "best fit"), options (a Record), required (one of date, time, year-month, month-day, or any), defaults (one of date, time, year-month, month-day, zoned-date-time, or all), and inherit (either all or relevant) and returns either a DateTime Format Record or null. It selects the best matching DateTime Format Record from formats based on matcher, options, required, defaults, and inherit. It performs the following steps when called:

1. If required is date, then
   1. Let requiredOptions be « "weekday", "year", "month", "day" ».
2. Else if required is time, then
   1. Let requiredOptions be « "dayPeriod", "hour", "minute", "second", "fractionalSecondDigits" ».
3. Else if required is year-month, then
   1. Let requiredOptions be « "year", "month" ».
4. Else if required is month-day, then
   1. Let requiredOptions be « "month", "day" ».
5. Else,
   1. Assert: required is any.
   2. Let requiredOptions be « "weekday", "year", "month", "day", "dayPeriod", "hour", "minute", "second", "fractionalSecondDigits" ».
6. If defaults is date, then
   1. Let defaultOptions be « "year", "month", "day" ».
7. Else if defaults is time, then
   1. Let defaultOptions be « "hour", "minute", "second" ».
8. Else if defaults is year-month, then
   1. Let defaultOptions be « "year", "month" ».
9. Else if defaults is month-day, then
   1. Let defaultOptions be « "month", "day" ».
10. Else,
    1. Assert: defaults is zoned-date-time or all.
    2. Let defaultOptions be « "year", "month", "day", "hour", "minute", "second" ».
11. If inherit is all, then
    1. Let formatOptions be a copy of options.
12. Else,
    1. Let formatOptions be a new Record.
    2. If required is one of date, year-month, or any, then
    3. Set formatOptions.[[era]] to options.[[era]].
       3. If required is either time or any, then
    4. Set formatOptions.[[hourCycle]] to options.[[hourCycle]].
13. Let anyPresent be false.
14. For each property name prop of « "weekday", "year", "month", "day", "dayPeriod", "hour", "minute", "second", "fractionalSecondDigits" », do
    1. If options.[[<prop>]] is not undefined, set anyPresent to true.
15. Let needDefaults be true.
16. For each property name prop of requiredOptions, do
    1. Let value be options.[[<prop>]].
    2. If value is not undefined, then
    3. Set formatOptions.[[<prop>]] to value.
    4. Set needDefaults to false.
17. If needDefaults is true, then
    1. If anyPresent is true and inherit is relevant, return null.
    2. For each property name prop of defaultOptions, do
    3. Set formatOptions.[[<prop>]] to "numeric".
       3. If defaults is zoned-date-time and formatOptions.[[timeZoneName]] is undefined, then
    4. Set formatOptions.[[timeZoneName]] to "short".
18. If matcher is "basic", then
    1. Let bestFormat be BasicFormatMatcher(formatOptions, formats).
19. Else,
    1. Let bestFormat be BestFitFormatMatcher(formatOptions, formats).
20. Return bestFormat.

# 15.6.2 AdjustDateTimeStyleFormat ( formats, baseFormat, matcher, allowedOptions )

The abstract operation AdjustDateTimeStyleFormat takes arguments formats (a List of DateTime Format Records), baseFormat (a DateTime Format Record), matcher (either "basic" or "best fit"), and allowedOptions (a List of property names from the "Property" column of Table 16) and returns a DateTime Format Record. It inspects baseFormat and determines the closest format to it that only includes fields from allowedOptions. This is used for determining the best format for Temporal objects with the `"dateStyle"` or `"timeStyle"` options. For example, a locale's best format for `"dateStyle": "full"` might include the weekday, which is not applicable when formatting a `Temporal.PlainYearMonth` object. It performs the following steps when called:

1. Let anyConflictingFields be false.
2. For each row of Table 16, except the header row, in table order, do
   1. Let prop be the name given in the "Property" column of the current row.
   2. If baseFormat has a [[<prop>]] field and allowedOptions does not contain prop, set anyConflictingFields to true.
3. If anyConflictingFields is false, return baseFormat.
4. NOTE: The above steps prevent the operation from returning an altered format when baseFormat would be sufficient. This should be unnecessary, but exists because the ECMA-402 specification does not guarantee that a format returned from DateTimeStyleFormat can also be returned from BasicFormatMatcher or BestFitFormatMatcher.
5. Let formatOptions be a new Record.
6. For each property name prop of allowedOptions, do
   1. If baseFormat has a [[<prop>]] field, set formatOptions.[[<prop>]] to baseFormat.[[<prop>]].
7. If matcher is "basic", then
   1. Let bestFormat be BasicFormatMatcher(formatOptions, formats).
8. Else,
   1. Let bestFormat be BestFitFormatMatcher(formatOptions, formats).
9. Return bestFormat.

# 15.6.3 DateTime Format Functions

A DateTime format function is an anonymous built-in function that has a [[DateTimeFormat]] internal slot.

When a DateTime format function F is called with optional argument date, the following steps are taken:

1. Let dtf be F.[[DateTimeFormat]].
2. Assert: dtf is an Object and dtf has an [[InitializedDateTimeFormat]] internal slot.
3. If date is not provided or is undefined, then
   1. Let x be ! Call(%Date.now%, undefined).
4. Else,
   1. Let x be ? ~~ToNumber~~ToDateTimeFormattable(date).
5. Return ? FormatDateTime(dtf, x).

The "length" property of a DateTime format function is 1𝔽.

# 15.6.4 FormatDateTimePattern ( dateTimeFormat, format, pattern, epochNanoseconds, isPlain )

The abstract operation FormatDateTimePattern takes arguments dateTimeFormat (an Intl.DateTimeFormat), format (a DateTime Format Record or a DateTime Range Pattern Format Record), pattern (a Pattern String), epochNanoseconds (a BigInt), and isPlain (a Boolean) and returns a List of Records with fields [[Type]] (a String) and [[Value]] (a String). It creates the corresponding parts for the epoch time epochNanoseconds according to pattern and to the effective locale and the formatting options of dateTimeFormat and format, with isPlain signifying whether epochNanoseconds is an epoch-time representation of a wall-clock time. It performs the following steps when called:

1. Let locale be dateTimeFormat.[[Locale]].
2. Let nfOptions be OrdinaryObjectCreate(null).
3. Perform ! CreateDataPropertyOrThrow(nfOptions, "numberingSystem", dateTimeFormat.[[NumberingSystem]]).
4. Perform ! CreateDataPropertyOrThrow(nfOptions, "useGrouping", false).
5. Let nf be ! Construct(%Intl.NumberFormat%, « locale, nfOptions »).
6. Let nf2Options be OrdinaryObjectCreate(null).
7. Perform ! CreateDataPropertyOrThrow(nf2Options, "minimumIntegerDigits", 2𝔽).
8. Perform ! CreateDataPropertyOrThrow(nf2Options, "numberingSystem", dateTimeFormat.[[NumberingSystem]]).
9. Perform ! CreateDataPropertyOrThrow(nf2Options, "useGrouping", false).
10. Let nf2 be ! Construct(%Intl.NumberFormat%, « locale, nf2Options »).
11. If format has a field [[fractionalSecondDigits]], then
    1. Let fractionalSecondDigits be format.[[fractionalSecondDigits]].
    2. Let nf3Options be OrdinaryObjectCreate(null).
    3. Perform ! CreateDataPropertyOrThrow(nf3Options, "minimumIntegerDigits", 𝔽(fractionalSecondDigits)).
    4. Perform ! CreateDataPropertyOrThrow(nf3Options, "numberingSystem", dateTimeFormat.[[NumberingSystem]]).
    5. Perform ! CreateDataPropertyOrThrow(nf3Options, "useGrouping", false).
    6. Let nf3 be ! Construct(%Intl.NumberFormat%, « locale, nf3Options »).
12. If isPlain is true, let timeZone be "+00:00"; else let timeZone be dateTimeFormat.[[TimeZone]].
13. Let tm be ToLocalTime(epochNanoseconds, dateTimeFormat.[[Calendar]], ~~dateTimeFormat.[[TimeZone]]~~timeZone).
14. Let patternParts be PartitionPattern(pattern).
15. Let result be a new empty List.
16. For each Record { [[Type]], [[Value]] } patternPart of patternParts, do
    1. Let p be patternPart.[[Type]].
    2. If p is "literal", then
    3. Append the Record { [[Type]]: "literal", [[Value]]: patternPart.[[Value]] } to result.
       3. Else if p is "fractionalSecondDigits", then
    4. Assert: format has a field [[fractionalSecondDigits]].
    5. Let v be tm.[[Millisecond]].
    6. Set v to floor(v × 10( fractionalSecondDigits \- 3 )).
    7. Let fv be FormatNumeric(nf3, v).
    8. Append the Record { [[Type]]: "fractionalSecond", [[Value]]: fv } to result.
       4. Else if p is "dayPeriod", then
    9. Assert: format has a field [[dayPeriod]].
    10. Let f be format.[[dayPeriod]].
    11. Let fv be a String value representing the day period of tm in the form given by f; the String value depends upon the implementation and the effective locale of dateTimeFormat.
    12. Append the Record { [[Type]]: p, [[Value]]: fv } to result.
        5. Else if p is "timeZoneName", then
    13. Assert: format has a field [[timeZoneName]].
    14. Let f be format.[[timeZoneName]].
    15. Let v be dateTimeFormat.[[TimeZone]].
    16. Let fv be a String value representing v in the form given by f; the String value depends upon the implementation and the effective locale of dateTimeFormat. The String value may also depend on the value of the [[InDST]] field of tm if f is "short", "long", "shortOffset", or "longOffset". If the implementation does not have such a localized representation of f, then use the String value of v itself.
    17. Append the Record { [[Type]]: p, [[Value]]: fv } to result.
        6. Else if p matches a Property column of the row in Table 16, then
    18. Assert: format has a field [[<p>]].
    19. Let f be format.[[<p>]].
    20. Let v be the value of tm's field whose name is the Internal Slot column of the matching row.
    21. If p is "year" and v ≤ 0, set v to 1 - v.
    22. If p is "month", set v to v \+ 1.
    23. If p is "hour" and dateTimeFormat.[[HourCycle]] is "h11" or "h12", then
        1. Set v to v modulo 12.
        2. If v is 0 and dateTimeFormat.[[HourCycle]] is "h12", set v to 12.
    24. If p is "hour" and dateTimeFormat.[[HourCycle]] is "h24", then
        1. If v is 0, set v to 24.
    25. If f is "numeric", then
        1. Let fv be FormatNumeric(nf, v).
    26. Else if f is "2-digit", then
        1. Let fv be FormatNumeric(nf2, v).
        2. Let codePoints be StringToCodePoints(fv).
        3. Let count be the number of elements in codePoints.
        4. If count > 2, then
           1. Let tens be codePoints[count \- 2].
           2. Let ones be codePoints[count \- 1].
           3. Set fv to CodePointsToString(« tens, ones »).
    27. Else if f is "narrow", "short", or "long", then
        1. Let fv be a String value representing v in the form given by f; the String value depends upon the implementation and the effective locale and calendar of dateTimeFormat. If p is "month", then the String value may also depend on whether format.[[day]] is present. If the implementation does not have a localized representation of f, then use the String value of v itself.
    28. Append the Record { [[Type]]: p, [[Value]]: fv } to result.
        7. Else if p is "ampm", then
    29. Let v be tm.[[Hour]].
    30. If v is greater than 11, then
        1. Let fv be an ILD String value representing "post meridiem".
    31. Else,
        1. Let fv be an ILD String value representing "ante meridiem".
    32. Append the Record { [[Type]]: "dayPeriod", [[Value]]: fv } to result.
        8. Else if p is "relatedYear", then
    33. Let v be tm.[[RelatedYear]].
    34. Let fv be FormatNumeric(nf, v).
    35. Append the Record { [[Type]]: "relatedYear", [[Value]]: fv } to result.
        9. Else if p is "yearName", then
    36. Let v be tm.[[YearName]].
    37. Let fv be an ILD String value representing v.
    38. Append the Record { [[Type]]: "yearName", [[Value]]: fv } to result.
        10. Else,
    39. Let unknown be an implementation-, locale-, and numbering system-dependent String based on epochNanoseconds and p.
    40. Append the Record { [[Type]]: "unknown", [[Value]]: unknown } to result.
17. Return result.

# 15.6.5 PartitionDateTimePattern ( dateTimeFormat, x )

The abstract operation PartitionDateTimePattern takes arguments dateTimeFormat (an Intl.DateTimeFormat) and x (a Number or an Object for which IsTemporalObject returns true) and returns either a normal completion containing a List of Records with fields [[Type]] (a String) and [[Value]] (a String), or a throw completion. It interprets x as ~~atime value as specified in es2024, 21.4.1.1~~an epoch time, and creates the corresponding parts according to the type of x, effective locale, and the formatting options of dateTimeFormat. It performs the following steps when called:

1. ~~Let x be TimeClip(x).~~
2. ~~If x is NaN, throw a RangeError exception.~~
3. ~~Let epochNanoseconds be ℤ(ℝ(x) × 106).~~
4. ~~Let format be dateTimeFormat.[[DateTimeFormat]].~~
5. Let formatRecord be ? HandleDateTimeValue(dateTimeFormat, x).
6. Let epochNanoseconds be formatRecord.[[EpochNanoseconds]].
7. Let format be formatRecord.[[Format]].
8. If format has a field [[hour]] and dateTimeFormat.[[HourCycle]] is "h11" or "h12", then
   1. Let pattern be format.[[pattern12]].
9. Else,
   1. Let pattern be format.[[pattern]].
10. Let result be FormatDateTimePattern(dateTimeFormat, format, pattern, epochNanoseconds, formatRecord.[[IsPlain]]).
11. Return result.

# 15.6.6 FormatDateTime ( dateTimeFormat, x )

The abstract operation FormatDateTime takes arguments dateTimeFormat (an Intl.DateTimeFormat) and x (a Number or an Object for which IsTemporalObject returns true) and returns either a normal completion containing a String or a throw completion. It performs the following steps when called:

1. Let parts be ? PartitionDateTimePattern(dateTimeFormat, x).
2. Let result be the empty String.
3. For each Record { [[Type]], [[Value]] } part of parts, do
   1. Set result to the string-concatenation of result and part.[[Value]].
4. Return result.

# 15.6.7 FormatDateTimeToParts ( dateTimeFormat, x )

The abstract operation FormatDateTimeToParts takes arguments dateTimeFormat (an Intl.DateTimeFormat) and x (a Number or an Object for which IsTemporalObject returns true) and returns either a normal completion containing an Array or a throw completion. It performs the following steps when called:

1. Let parts be ? PartitionDateTimePattern(dateTimeFormat, x).
2. Let result be ! ArrayCreate(0).
3. Let n be 0.
4. For each Record { [[Type]], [[Value]] } part of parts, do
   1. Let O be OrdinaryObjectCreate(%Object.prototype%).
   2. Perform ! CreateDataPropertyOrThrow(O, "type", part.[[Type]]).
   3. Perform ! CreateDataPropertyOrThrow(O, "value", part.[[Value]]).
   4. Perform ! CreateDataPropertyOrThrow(result, ! ToString(𝔽(n)), O).
   5. Increment n by 1.
5. Return result.

# 15.6.8 PartitionDateTimeRangePattern ( dateTimeFormat, x, y )

The abstract operation PartitionDateTimeRangePattern takes arguments dateTimeFormat (an Intl.DateTimeFormat), x (a Number or an Object for which IsTemporalObject returns true), and y (a Number or an Object for which IsTemporalObject returns true) and returns either a normal completion containing a List of Records with fields [[Type]] (a String), [[Value]] (a String), and [[Source]] (a String), or a throw completion. It interprets x and y ~~atime value as specified in es2024, 21.4.1.1~~epoch times, and creates the corresponding parts according to the types of x and y, effective locale, and the formatting options of dateTimeFormat. It performs the following steps when called:

1. ~~Let x be TimeClip(x).~~
2. ~~If x is NaN, throw a RangeError exception.~~
3. ~~Let y be TimeClip(y).~~
4. ~~If y is NaN, throw a RangeError exception.~~
5. ~~Let xEpochNanoseconds be ℤ(ℝ(x) × 106).~~
6. ~~Let yEpochNanoseconds be ℤ(ℝ(y) × 106).~~
7. If IsTemporalObject(x) is true or IsTemporalObject(y) is true, then
   1. If SameTemporalType(x, y) is false, throw a TypeError exception.
8. Let xFormatRecord be ? HandleDateTimeValue(dateTimeFormat, x).
9. Let yFormatRecord be ? HandleDateTimeValue(dateTimeFormat, y).
10. Let xEpochNanoseconds be xFormatRecord.[[EpochNanoseconds]].
11. Let yEpochNanoseconds be yFormatRecord.[[EpochNanoseconds]].
12. Let tm1 be ToLocalTime(xEpochNanoseconds, dateTimeFormat.[[Calendar]], dateTimeFormat.[[TimeZone]]).
13. Let tm2 be ToLocalTime(yEpochNanoseconds, dateTimeFormat.[[Calendar]], dateTimeFormat.[[TimeZone]]).
14. ~~Let format be dateTimeFormat.[[DateTimeFormat]].~~
15. Let format be xFormatRecord.[[Format]].
16. Assert: format is equal to yFormatRecord.[[Format]].
17. Assert: xFormatRecord.[[IsPlain]] = yFormatRecord.[[IsPlain]].
18. If format has a field [[hour]] and dateTimeFormat.[[HourCycle]] is "h11" or "h12", then
    1. Let pattern be format.[[pattern12]].
    2. Let rangePatterns be format.[[rangePatterns12]].
19. Else,
    1. Let pattern be format.[[pattern]].
    2. Let rangePatterns be format.[[rangePatterns]].
20. Let selectedRangePattern be undefined.
21. Let relevantFieldsEqual be true.
22. Let checkMoreFields be true.
23. For each row of Table 6, except the header row, in table order, do
    1. Let fieldName be the name given in the Field Name column of the row.
    2. If rangePatterns has a field whose name is fieldName, let rangePattern be rangePatterns' field whose name is fieldName; else let rangePattern be undefined.
    3. If selectedRangePattern is not undefined and rangePattern is undefined, then
    4. NOTE: Because there is no range pattern for differences at or below this field, no further checks will be performed.
    5. Set checkMoreFields to false.
       4. If fieldName is not equal to [[Default]] and relevantFieldsEqual is true and checkMoreFields is true, then
    6. Set selectedRangePattern to rangePattern.
    7. If fieldName is [[AmPm]], then
       1. If tm1.[[Hour]] is less than 12, let v1 be "am"; else let v1 be "pm".
       2. If tm2.[[Hour]] is less than 12, let v2 be "am"; else let v2 be "pm".
    8. Else if fieldName is [[DayPeriod]], then
       1. Let v1 be a String value representing the day period of tm1; the String value depends upon the implementation and the effective locale of dateTimeFormat.
       2. Let v2 be a String value representing the day period of tm2; the String value depends upon the implementation and the effective locale of dateTimeFormat.
    9. Else if fieldName is [[FractionalSecondDigits]], then
       1. Let fractionalSecondDigits be dateTimeFormat.[[FractionalSecondDigits]].
       2. If fractionalSecondDigits is undefined, then
          1. Set fractionalSecondDigits to 3.
       3. Let exp be fractionalSecondDigits \- 3.
       4. Let v1 be floor(tm1.[[Millisecond]] × 10exp).
       5. Let v2 be floor(tm2.[[Millisecond]] × 10exp).
    10. Else,
        1. Let v1 be tm1's field whose name is fieldName.
        2. Let v2 be tm2's field whose name is fieldName.
    11. If v1 is not equal to v2, then
        1. Set relevantFieldsEqual to false.
24. If relevantFieldsEqual is true, then
    1. Let collapsedResult be a new empty List.
    2. Let resultParts be FormatDateTimePattern(dateTimeFormat, format, pattern, xEpochNanoseconds, xFormatRecord.[[IsPlain]]).
    3. For each Record { [[Type]], [[Value]] } r of resultParts, do
    4. Append the Record { [[Type]]: r.[[Type]], [[Value]]: r.[[Value]], [[Source]]: "shared" } to collapsedResult.
       4. Return collapsedResult.
25. Let rangeResult be a new empty List.
26. If selectedRangePattern is undefined, then
    1. Set selectedRangePattern to rangePatterns.[[Default]].
27. For each Record { [[Pattern]], [[Source]] } rangePatternPart of selectedRangePattern.[[PatternParts]], do
    1. Let pattern be rangePatternPart.[[Pattern]].
    2. Let source be rangePatternPart.[[Source]].
    3. If source is "startRange" or "shared", then
    4. Let z be xEpochNanoseconds.
       4. Else,
    5. Let z be yEpochNanoseconds.
       5. Let resultParts be FormatDateTimePattern(dateTimeFormat, selectedRangePattern, pattern, z, xFormatRecord.[[IsPlain]]).
       6. For each Record { [[Type]], [[Value]] } r of resultParts, do
    6. Append the Record { [[Type]]: r.[[Type]], [[Value]]: r.[[Value]], [[Source]]: source } to rangeResult.
28. Return rangeResult.

# 15.6.9 FormatDateTimeRange ( dateTimeFormat, x, y )

The abstract operation FormatDateTimeRange takes arguments dateTimeFormat (an Intl.DateTimeFormat), x (a Number or an Object for which IsTemporalObject returns true), and y (a Number or an Object for which IsTemporalObject returns true) and returns either a normal completion containing a String or a throw completion. It performs the following steps when called:

1. Let parts be ? PartitionDateTimeRangePattern(dateTimeFormat, x, y).
2. Let result be the empty String.
3. For each Record { [[Type]], [[Value]], [[Source]] } part of parts, do
   1. Set result to the string-concatenation of result and part.[[Value]].
4. Return result.

# 15.6.10 FormatDateTimeRangeToParts ( dateTimeFormat, x, y )

The abstract operation FormatDateTimeRangeToParts takes arguments dateTimeFormat (an Intl.DateTimeFormat), x (a Number or an Object for which IsTemporalObject returns true), and y (a Number or an Object for which IsTemporalObject returns true) and returns either a normal completion containing an Array or a throw completion. It performs the following steps when called:

1. Let parts be ? PartitionDateTimeRangePattern(dateTimeFormat, x, y).
2. Let result be ! ArrayCreate(0).
3. Let n be 0.
4. For each Record { [[Type]], [[Value]], [[Source]] } part of parts, do
   1. Let O be OrdinaryObjectCreate(%Object.prototype%).
   2. Perform ! CreateDataPropertyOrThrow(O, "type", part.[[Type]]).
   3. Perform ! CreateDataPropertyOrThrow(O, "value", part.[[Value]]).
   4. Perform ! CreateDataPropertyOrThrow(O, "source", part.[[Source]]).
   5. Perform ! CreateDataPropertyOrThrow(result, ! ToString(𝔽(n)), O).
   6. Increment n by 1.
5. Return result.

# 15.6.11 ToDateTimeFormattable ( value )

The abstract operation ToDateTimeFormattable takes argument value (an ECMAScript language value, but not undefined) and returns either a normal completion containing either a Number or an Object for which IsTemporalObject returns true, or a throw completion. It converts value to a value that can be formatted by an %Intl.DateTimeFormat% object. It performs the following steps when called:

1. If IsTemporalObject(value) is true, return value.
2. Return ? ToNumber(value).

# 15.6.12 IsTemporalObject ( value )

The abstract operation IsTemporalObject takes argument value (an ECMAScript language value) and returns a Boolean. It returns true if value is an instance of a Temporal type that can be formatted. It performs the following steps when called:

1. If value is not an Object, return false.
2. If value does not have an [[InitializedTemporalDate]], [[InitializedTemporalTime]], [[InitializedTemporalDateTime]], [[InitializedTemporalZonedDateTime]], [[InitializedTemporalYearMonth]], [[InitializedTemporalMonthDay]], or [[InitializedTemporalInstant]] internal slot, return false.
3. Return true.

# 15.6.13 SameTemporalType ( x, y )

The abstract operation SameTemporalType takes arguments x (an ECMAScript language value) and y (an ECMAScript language value) and returns a Boolean. It determines whether x and y are both instances of the same Temporal type. It performs the following steps when called:

1. If either of IsTemporalObject(x) or IsTemporalObject(y) is false, return false.
2. If x has an [[InitializedTemporalDate]] internal slot and y does not, return false.
3. If x has an [[InitializedTemporalTime]] internal slot and y does not, return false.
4. If x has an [[InitializedTemporalDateTime]] internal slot and y does not, return false.
5. If x has an [[InitializedTemporalZonedDateTime]] internal slot and y does not, return false.
6. If x has an [[InitializedTemporalYearMonth]] internal slot and y does not, return false.
7. If x has an [[InitializedTemporalMonthDay]] internal slot and y does not, return false.
8. If x has an [[InitializedTemporalInstant]] internal slot and y does not, return false.
9. Return true.

# 15.6.14 Value Format Records

Each Value Format Record has the fields defined in Table 31.

| Table 31: Record returned by HandleDateTimeValue Field Name | Value Type               |
| ----------------------------------------------------------- | ------------------------ |
| [[Format]]                                                  | a DateTime Format Record |
| [[EpochNanoseconds]]                                        | a BigInt                 |
| [[IsPlain]]                                                 | a Boolean                |

# 15.6.15 HandleDateTimeTemporalDate ( dateTimeFormat, temporalDate )

The abstract operation HandleDateTimeTemporalDate takes arguments dateTimeFormat (an Intl.DateTimeFormat) and temporalDate (a Temporal.PlainDate) and returns either a normal completion containing a Value Format Record or a throw completion. It prepares temporalDate for formatting by dateTimeFormat, returning a Value Format Record, or throws if the calendars are incompatible or dateTimeFormat has no suitable format. It performs the following steps when called:

1. If temporalDate.[[Calendar]] is not either dateTimeFormat.[[Calendar]] or "iso8601", throw a RangeError exception.
2. Let isoDateTime be CombineISODateAndTimeRecord(temporalDate.[[ISODate]], NoonTimeRecord()).
3. Let epochNs be GetUTCEpochNanoseconds(isoDateTime).
4. Let format be dateTimeFormat.[[TemporalPlainDateFormat]].
5. If format is null, throw a TypeError exception.
6. Return Value Format Record { [[Format]]: format, [[EpochNanoseconds]]: epochNs, [[IsPlain]]: true }.

# 15.6.16 HandleDateTimeTemporalYearMonth ( dateTimeFormat, temporalYearMonth )

The abstract operation HandleDateTimeTemporalYearMonth takes arguments dateTimeFormat (an Intl.DateTimeFormat) and temporalYearMonth (a Temporal.PlainYearMonth) and returns either a normal completion containing a Value Format Record or a throw completion. It prepares temporalYearMonth for formatting by dateTimeFormat, returning a Value Format Record, or throws if the calendars are incompatible or dateTimeFormat has no suitable format. It performs the following steps when called:

1. If temporalYearMonth.[[Calendar]] is not equal to dateTimeFormat.[[Calendar]], throw a RangeError exception.
2. Let isoDateTime be CombineISODateAndTimeRecord(temporalYearMonth.[[ISODate]], NoonTimeRecord()).
3. Let epochNs be GetUTCEpochNanoseconds(isoDateTime).
4. Let format be dateTimeFormat.[[TemporalPlainYearMonthFormat]].
5. If format is null, throw a TypeError exception.
6. Return Value Format Record { [[Format]]: format, [[EpochNanoseconds]]: epochNs, [[IsPlain]]: true }.

# 15.6.17 HandleDateTimeTemporalMonthDay ( dateTimeFormat, temporalMonthDay )

The abstract operation HandleDateTimeTemporalMonthDay takes arguments dateTimeFormat (an Intl.DateTimeFormat) and temporalMonthDay (a Temporal.PlainMonthDay) and returns either a normal completion containing a Value Format Record or a throw completion. It prepares temporalMonthDay for formatting by dateTimeFormat, returning a Value Format Record, or throws if the calendars are incompatible or dateTimeFormat has no suitable format. It performs the following steps when called:

1. If temporalMonthDay.[[Calendar]] is not equal to dateTimeFormat.[[Calendar]], throw a RangeError exception.
2. Let isoDateTime be CombineISODateAndTimeRecord(temporalMonthDay.[[ISODate]], NoonTimeRecord()).
3. Let epochNs be GetUTCEpochNanoseconds(isoDateTime).
4. Let format be dateTimeFormat.[[TemporalPlainMonthDayFormat]].
5. If format is null, throw a TypeError exception.
6. Return Value Format Record { [[Format]]: format, [[EpochNanoseconds]]: epochNs, [[IsPlain]]: true }.

# 15.6.18 HandleDateTimeTemporalTime ( dateTimeFormat, temporalTime )

The abstract operation HandleDateTimeTemporalTime takes arguments dateTimeFormat (an Intl.DateTimeFormat) and temporalTime (a Temporal.PlainTime) and returns either a normal completion containing a Value Format Record or a throw completion. It prepares temporalTime for formatting by dateTimeFormat, returning a Value Format Record, or throws if dateTimeFormat has no suitable format. It performs the following steps when called:

1. Let isoDate be CreateISODateRecord(1970, 1, 1).
2. Let isoDateTime be CombineISODateAndTimeRecord(isoDate, temporalTime.[[Time]]).
3. Let epochNs be GetUTCEpochNanoseconds(isoDateTime).
4. Let format be dateTimeFormat.[[TemporalPlainTimeFormat]].
5. If format is null, throw a TypeError exception.
6. Return Value Format Record { [[Format]]: format, [[EpochNanoseconds]]: epochNs, [[IsPlain]]: true }.

# 15.6.19 HandleDateTimeTemporalDateTime ( dateTimeFormat, dateTime )

The abstract operation HandleDateTimeTemporalDateTime takes arguments dateTimeFormat (an Intl.DateTimeFormat) and dateTime (a Temporal.PlainDateTime) and returns either a normal completion containing a Value Format Record or a throw completion. It prepares dateTime for formatting by dateTimeFormat, returning a Value Format Record, or throws if the calendars are incompatible. It performs the following steps when called:

1. If dateTime.[[Calendar]] is not "iso8601" and not equal to dateTimeFormat.[[Calendar]], throw a RangeError exception.
2. Let epochNs be GetUTCEpochNanoseconds(dateTime.[[ISODateTime]]).
3. Let format be dateTimeFormat.[[TemporalPlainDateTimeFormat]].
4. Return Value Format Record { [[Format]]: format, [[EpochNanoseconds]]: epochNs, [[IsPlain]]: true }.

# 15.6.20 HandleDateTimeTemporalInstant ( dateTimeFormat, instant )

The abstract operation HandleDateTimeTemporalInstant takes arguments dateTimeFormat (an Intl.DateTimeFormat) and instant (a Temporal.Instant) and returns a Value Format Record. It prepares instant for formatting by dateTimeFormat, returning a Value Format Record. It performs the following steps when called:

1. Let format be dateTimeFormat.[[TemporalInstantFormat]].
2. Return Value Format Record { [[Format]]: format, [[EpochNanoseconds]]: instant.[[EpochNanoseconds]], [[IsPlain]]: false }.

# 15.6.21 HandleDateTimeOthers ( dateTimeFormat, x )

The abstract operation HandleDateTimeOthers takes arguments dateTimeFormat (an Intl.DateTimeFormat) and x (a Number) and returns either a normal completion containing a Value Format Record or a throw completion. It prepares the Number value x for formatting by dateTimeFormat, returning a Value Format Record, or throws if x is not a finite time value. It performs the following steps when called:

1. Set x to TimeClip(x).
2. If x is NaN, throw a RangeError exception.
3. Let epochNanoseconds be ℤ(ℝ(x) × 106).
4. Let format be dateTimeFormat.[[DateTimeFormat]].
5. Return Value Format Record { [[Format]]: format, [[EpochNanoseconds]]: epochNanoseconds, [[IsPlain]]: false }.

# 15.6.22 HandleDateTimeValue ( dateTimeFormat, x )

The abstract operation HandleDateTimeValue takes arguments dateTimeFormat (an Intl.DateTimeFormat) and x (either a Number, or an Object for which IsTemporalObject returns true) and returns either a normal completion containing a Value Format Record or a throw completion. It dispatches x to the appropriate handler based on its type for formatting by dateTimeFormat. It performs the following steps when called:

1. If x is a Number, return ? HandleDateTimeOthers(dateTimeFormat, x).
2. If x has an [[InitializedTemporalDate]] internal slot, return ? HandleDateTimeTemporalDate(dateTimeFormat, x).
3. If x has an [[InitializedTemporalYearMonth]] internal slot, return ? HandleDateTimeTemporalYearMonth(dateTimeFormat, x).
4. If x has an [[InitializedTemporalMonthDay]] internal slot, return ? HandleDateTimeTemporalMonthDay(dateTimeFormat, x).
5. If x has an [[InitializedTemporalTime]] internal slot, return ? HandleDateTimeTemporalTime(dateTimeFormat, x).
6. If x has an [[InitializedTemporalDateTime]] internal slot, return ? HandleDateTimeTemporalDateTime(dateTimeFormat, x).
7. If x has an [[InitializedTemporalInstant]] internal slot, return HandleDateTimeTemporalInstant(dateTimeFormat, x).
8. Assert: x has an [[InitializedTemporalZonedDateTime]] internal slot.
9. Throw a TypeError exception.

# 15.6.23 ToLocalTime ( epochNs, calendar, timeZoneIdentifier )

The implementation-defined abstract operation ToLocalTime takes arguments epochNs (a BigInt), calendar (a String), and timeZoneIdentifier (a String) and returns a ToLocalTime Record. It performs the following steps when called:

1. If IsTimeZoneOffsetString(timeZoneIdentifier) is true, then
   1. Let offsetNs be ~~ParseTimeZoneOffsetString~~! ParseDateTimeUTCOffset(timeZoneIdentifier).
2. Else,
   1. Assert: GetAvailableNamedTimeZoneIdentifier(timeZoneIdentifier) is not empty.
   2. Let offsetNs be GetNamedTimeZoneOffsetNanoseconds(timeZoneIdentifier, epochNs).
3. Let tz be ℝ(epochNs) + offsetNs.
4. If calendar is "gregory", then
   1. Return a ToLocalTime Record with fields calculated from tz according to Table 17.
5. Else,
   1. Return a ToLocalTime Record with the fields calculated from tz for the given calendar. The calculations should use best available information about the specified calendar. Given the same values of epochNs, calendar, and timeZoneIdentifier, the result must be the same for the lifetime of the surrounding agent.

Note 1

A conforming implementation must recognize "UTC" and all Zone and Link names from the IANA Time Zone Database (and **only** such names), and use best available current and historical information about their offsets from UTC and their daylight saving time rules in calculations.

Note 2

Time zone information is subject to change, and host environments may update their time zone database at any time. At a minimum, implementations must ensure that the time zone information for each particular value of timeZone individually remains constant starting from the time it is first accessed, for the lifetime of the surrounding agent. Furthermore, it is recommended that the time zone information for all values of timeZone as a whole (i.e. the time zone database) remains the same for the lifetime of the surrounding agent.

# 15.7 Properties of the Intl.DateTimeFormat Prototype Object

# 15.7.1 Intl.DateTimeFormat.prototype.formatToParts ( date )

When the `formatToParts` method is called with an argument date, the following steps are taken:

1. Let dtf be the this value.
2. Perform ? RequireInternalSlot(dtf, [[InitializedDateTimeFormat]]).
3. If date is undefined, then
   1. Let x be ! Call(%Date.now%, undefined).
4. Else,
   1. Let x be ? ~~ToNumber~~ToDateTimeFormattable(date).
5. Return ? FormatDateTimeToParts(dtf, x).

# 15.7.2 Intl.DateTimeFormat.prototype.formatRange ( startDate, endDate )

When the `formatRange` method is called with an arguments startDate and endDate, the following steps are taken:

1. Let dtf be this value.
2. Perform ? RequireInternalSlot(dtf, [[InitializedDateTimeFormat]]).
3. If startDate is undefined or endDate is undefined, throw a TypeError exception.
4. Let x be ? ~~ToNumber~~ToDateTimeFormattable(startDate).
5. Let y be ? ~~ToNumber~~ToDateTimeFormattable(endDate).
6. Return ? FormatDateTimeRange(dtf, x, y).

# 15.7.3 Intl.DateTimeFormat.prototype.formatRangeToParts ( startDate, endDate )

When the `formatRangeToParts` method is called with arguments startDate and endDate, the following steps are taken:

1. Let dtf be this value.
2. Perform ? RequireInternalSlot(dtf, [[InitializedDateTimeFormat]]).
3. If startDate is undefined or endDate is undefined, throw a TypeError exception.
4. Let x be ? ~~ToNumber~~ToDateTimeFormattable(startDate).
5. Let y be ? ~~ToNumber~~ToDateTimeFormattable(endDate).
6. Return ? FormatDateTimeRangeToParts(dtf, x, y).

# 15.8 Properties of Intl.DateTimeFormat Instances

Intl.DateTimeFormat instances are ordinary objects that inherit properties from %Intl.DateTimeFormat.prototype%.

Intl.DateTimeFormat instances have an [[InitializedDateTimeFormat]] internal slot.

Intl.DateTimeFormat instances also have several internal slots that are computed by the constructor:

- [[Locale]] is a String value with the language tag of the locale whose localization is used for formatting.
- [[Calendar]] is a String value representing the Unicode Calendar Identifier used for formatting.
- [[NumberingSystem]] is a String value representing the Unicode Number System Identifier used for formatting.
- [[TimeZone]] is a String value used for formatting that is ~~either a~~ an available time zone identifier ~~from the IANA Time Zone Database or a UTC offset in ISO 8601 extended format~~.
- [[HourCycle]] is a String value indicating whether the 12-hour format ("h11", "h12") or the 24-hour format ("h23", "h24") should be used. "h11" and "h23" start with hour 0 and go up to 11 and 23 respectively. "h12" and "h24" start with hour 1 and go up to 12 and 24.~~[[HourCycle]] is only used when [[DateTimeFormat]] has an [[hour]] field.~~
- [[DateStyle]], [[TimeStyle]] are each either undefined, or a String value with values "full", "long", "medium", or "short".
- [[DateTimeFormat]] is a DateTime Format Record.
- [[TemporalPlainDateFormat]] is a DateTime Format Record or null.
- [[TemporalPlainYearMonthFormat]] is a DateTime Format Record or null.
- [[TemporalPlainMonthDayFormat]] is a DateTime Format Record or null.
- [[TemporalPlainTimeFormat]] is a DateTime Format Record or null.
- [[TemporalPlainDateTimeFormat]] is a DateTime Format Record.
- [[TemporalInstantFormat]] is a DateTime Format Record.

Finally, Intl.DateTimeFormat instances have a [[BoundFormat]] internal slot that caches the function returned by the format accessor (11.3.3).

# 15.9 Abstract Operations for DurationFormat Objects

Editor's Note

The deleted definitions of ToIntegerIfIntegral, DurationSign, and IsValidDuration are moved into ECMA-262.

~~

# 15.9.1 Duration Records

A Duration Record is a Record value used to represent a Duration.

Duration Records have the fields listed in Table 32

| Table 32: Duration Record Fields Field | Meaning                                     |
| -------------------------------------- | ------------------------------------------- |
| [[Years]]                              | The number of years in the duration.        |
| [[Months]]                             | The number of months in the duration.       |
| [[Weeks]]                              | The number of weeks in the duration.        |
| [[Days]]                               | The number of days in the duration.         |
| [[Hours]]                              | The number of hours in the duration.        |
| [[Minutes]]                            | The number of minutes in the duration.      |
| [[Seconds]]                            | The number of seconds in the duration.      |
| [[Milliseconds]]                       | The number of milliseconds in the duration. |
| [[Microseconds]]                       | The number of microseconds in the duration. |
| [[Nanoseconds]]                        | The number of nanoseconds in the duration.  |
| ~~ ~~                                  |                                             |

# 15.9.2 ToIntegerIfIntegral ( argument )

The abstract operation ToIntegerIfIntegral takes argument argument (an ECMAScript language value) and returns either a normal completion containing an integer or a throw completion. It converts argument to an integer representing its Number value, or throws a RangeError when that value is not integral. It performs the following steps when called:

1. Let number be ? ToNumber(argument).
2. If number is not an integral Number, throw a RangeError exception.
3. Return ℝ(number).

~~ ~~

# 15.9.3 ToDurationRecord ( input )

The abstract operation ToDurationRecord takes argument input (an ECMAScript language value) and returns either a normal completion containing a Duration Record, or an abrupt completion. It converts a given object that represents a Duration into a Duration Record. It performs the following steps when called:

1. If input is not an Object, then
   1. If input is a String, throw a RangeError exception.
   2. Throw a TypeError exception.
2. Let result be a new Duration Record with each field set to 0.
3. Let days be ? Get(input, "days").
4. If days is not undefined, set result.[[Days]] to ? ToIntegerIfIntegral(days).
5. Let hours be ? Get(input, "hours").
6. If hours is not undefined, set result.[[Hours]] to ? ToIntegerIfIntegral(hours).
7. Let microseconds be ? Get(input, "microseconds").
8. If microseconds is not undefined, set result.[[Microseconds]] to ? ToIntegerIfIntegral(microseconds).
9. Let milliseconds be ? Get(input, "milliseconds").
10. If milliseconds is not undefined, set result.[[Milliseconds]] to ? ToIntegerIfIntegral(milliseconds).
11. Let minutes be ? Get(input, "minutes").
12. If minutes is not undefined, set result.[[Minutes]] to ? ToIntegerIfIntegral(minutes).
13. Let months be ? Get(input, "months").
14. If months is not undefined, set result.[[Months]] to ? ToIntegerIfIntegral(months).
15. Let nanoseconds be ? Get(input, "nanoseconds").
16. If nanoseconds is not undefined, set result.[[Nanoseconds]] to ? ToIntegerIfIntegral(nanoseconds).
17. Let seconds be ? Get(input, "seconds").
18. If seconds is not undefined, set result.[[Seconds]] to ? ToIntegerIfIntegral(seconds).
19. Let weeks be ? Get(input, "weeks").
20. If weeks is not undefined, set result.[[Weeks]] to ? ToIntegerIfIntegral(weeks).
21. Let years be ? Get(input, "years").
22. If years is not undefined, set result.[[Years]] to ? ToIntegerIfIntegral(years).
23. If years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, and nanoseconds are all undefined, throw a TypeError exception.
24. If IsValidDuration( result.[[Years]], result.[[Months]], result.[[Weeks]], result.[[Days]], result.[[Hours]], result.[[Minutes]], result.[[Seconds]], result.[[Milliseconds]], result.[[Microseconds]], result.[[Nanoseconds]]) is false, then
    1. Throw a RangeError exception.
25. Return result.

~~ ~~

# 15.9.4 DurationSign ( duration )

The abstract operation DurationSign takes argument duration (a Duration Record) and returns -1, 0, or 1. It returns 1 if the most significant non-zero element in its arguments is positive, and -1 if the most significant non-zero element is negative. If all of its arguments are zero, it returns 0. It performs the following steps when called:

1. For each value v of « duration.[[Years]], duration.[[Months]], duration.[[Weeks]], duration.[[Days]], duration.[[Hours]], duration.[[Minutes]], duration.[[Seconds]], duration.[[Milliseconds]], duration.[[Microseconds]], duration.[[Nanoseconds]] », do
   1. If v < 0, return -1.
   2. If v > 0, return 1.
2. Return 0.

~~ ~~

# 15.9.5 IsValidDuration ( years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds )

The abstract operation IsValidDuration takes arguments years (an integer), months (an integer), weeks (an integer), days (an integer), hours (an integer), minutes (an integer), seconds (an integer), milliseconds (an integer), microseconds (an integer), and nanoseconds (an integer) and returns a Boolean. It returns true if its arguments form valid input from which to construct a Duration Record, and false otherwise. It performs the following steps when called:

1. Let sign be 0.
2. For each value v of « years, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds, nanoseconds », do
   1. If 𝔽(v) is not finite, return false.
   2. If v < 0, then
      1. If sign > 0, return false.
      2. Set sign to -1.
   3. Else if v > 0, then
      1. If sign < 0, return false.
      2. Set sign to 1.
3. If abs(years) ≥ 232, return false.
4. If abs(months) ≥ 232, return false.
5. If abs(weeks) ≥ 232, return false.
6. Let normalizedSeconds be days × 86,400 + hours × 3600 + minutes × 60 + seconds \+ ℝ(𝔽(milliseconds)) × 10-3 \+ ℝ(𝔽(microseconds)) × 10-6 \+ ℝ(𝔽(nanoseconds)) × 10-9.
7. NOTE: The above step cannot be implemented directly using floating-point arithmetic. Multiplying by 10-3, 10-6, and 10-9 respectively may be imprecise when milliseconds, microseconds, or nanoseconds is an unsafe integer. This multiplication can be implemented in C++ with an implementation of `std::remquo()` with sufficient bits in the quotient. String manipulation will also give an exact result, since the multiplication is by a power of 10.
8. If abs(normalizedSeconds) ≥ 253, return false.
9. Return true.

~~

# 15.9.6 ComputeFractionalDigits ( durationFormat, duration )

The abstract operation ComputeFractionalDigits takes arguments durationFormat (a DurationFormat Object) and duration (a ~~Duration Record~~ Temporal.Duration) and returns a mathematical value. It computes the sum of all values in durationFormat units with "fractional" style, expressed as a fraction of the smallest unit of durationFormat that does not use "fractional" style. It performs the following steps when called:

1. Let result be 0.
2. Let exponent be 3.
3. For each row of Table 24, except the header row, in table order, do
   1. Let style be the value of durationFormat's internal slot whose name is the Style Slot value of the current row.
   2. If style is "fractional", then
      1. Assert: The Unit value of the current row is "milliseconds", "microseconds", or "nanoseconds".
      2. Let value be the value of duration's field whose name is the Value Field value of the current row.
      3. Set value to value / 10exponent.
      4. Set result to result \+ value.
      5. Set exponent to exponent \+ 3.
4. Return result.

# 15.9.7 FormatNumericUnits ( durationFormat, duration, firstNumericUnit, signDisplayed )

The abstract operation FormatNumericUnits takes arguments durationFormat (a DurationFormat Object), duration (a ~~Duration Record~~ Temporal.Duration), firstNumericUnit (a String), and signDisplayed (a Boolean) and returns a List of Records. It creates the parts representing the elements of duration that use "numeric" or "2-digit" style according to the effective locale and the formatting options of durationFormat. It performs the following steps when called:

1. Assert: firstNumericUnit is "hours", "minutes", or "seconds".
2. Let numericPartsList be a new empty List.
3. Let hoursValue be duration.[[Hours]].
4. Let hoursDisplay be durationFormat.[[HoursDisplay]].
5. Let minutesValue be duration.[[Minutes]].
6. Let minutesDisplay be durationFormat.[[MinutesDisplay]].
7. Let secondsValue be duration.[[Seconds]].
8. If duration.[[Milliseconds]] is not 0 or duration.[[Microseconds]] is not 0 or duration.[[Nanoseconds]] is not 0, then
   1. Set secondsValue to secondsValue \+ ComputeFractionalDigits(durationFormat, duration).
9. Let secondsDisplay be durationFormat.[[SecondsDisplay]].
10. Let hoursFormatted be false.
11. If firstNumericUnit is "hours", then
    1. If hoursValue is not 0 or hoursDisplay is "always", then
    1. Set hoursFormatted to true.
12. If secondsValue is not 0 or secondsDisplay is "always", then
    1. Let secondsFormatted be true.
13. Else,
    1. Let secondsFormatted be false.
14. Let minutesFormatted be false.
15. If firstNumericUnit is "hours" or firstNumericUnit is "minutes", then
    1. If hoursFormatted is true and secondsFormatted is true, then
    1. Set minutesFormatted to true.
       2. Else if minutesValue is not 0 or minutesDisplay is "always", then
    1. Set minutesFormatted to true.
16. If hoursFormatted is true, then
    1. If signDisplayed is true, then
    1. If hoursValue is 0 and DurationSign(duration) is -1, then
       1. Set hoursValue to negative-zero.
          2. Let hoursParts be FormatNumericHours(durationFormat, hoursValue, signDisplayed).
          3. Set numericPartsList to the list-concatenation of numericPartsList and hoursParts.
          4. Set signDisplayed to false.
17. If minutesFormatted is true, then
    1. If signDisplayed is true, then
    1. If minutesValue is 0 and DurationSign(duration) is -1, then
       1. Set minutesValue to negative-zero.
          2. Let minutesParts be FormatNumericMinutes(durationFormat, minutesValue, hoursFormatted, signDisplayed).
          3. Set numericPartsList to the list-concatenation of numericPartsList and minutesParts.
          4. Set signDisplayed to false.
18. If secondsFormatted is true, then
    1. Let secondsParts be FormatNumericSeconds(durationFormat, secondsValue, minutesFormatted, signDisplayed).
    2. Set numericPartsList to the list-concatenation of numericPartsList and secondsParts.
19. Return numericPartsList.

# 15.9.8 PartitionDurationFormatPattern ( durationFormat, duration )

The abstract operation PartitionDurationFormatPattern takes arguments durationFormat (a DurationFormat) and duration (a ~~Duration Record~~ Temporal.Duration) and returns a List. It creates the corresponding parts for duration according to the effective locale and the formatting options of durationFormat. It performs the following steps when called:

1. Let result be a new empty List.
2. Let signDisplayed be true.
3. Let numericUnitFound be false.
4. While numericUnitFound is false, repeat for each row in Table 24 in table order, except the header row:
   1. Let value be the value of duration's field whose name is the Value Field value of the current row.
   2. Let style be the value of durationFormat's internal slot whose name is the Style Slot value of the current row.
   3. Let display be the value of durationFormat's internal slot whose name is the Display Slot value of the current row.
   4. Let unit be the Unit value of the current row.
   5. If style is "numeric" or "2-digit", then
      1. Append FormatNumericUnits(durationFormat, duration, unit, signDisplayed) to result.
      2. Let numericPartsList be FormatNumericUnits(durationFormat, duration, unit, signDisplayed).
      3. If numericPartsList is not empty, append numericPartsList to result.
      4. Set numericUnitFound to true.
   6. Else,
      1. Let nfOpts be OrdinaryObjectCreate(null).
      2. If unit is "seconds", "milliseconds", or "microseconds", then
         1. If NextUnitFractional(durationFormat, unit) is true, then
            1. Set value to value \+ ComputeFractionalDigits(durationFormat, duration).
            2. If durationFormat.[[FractionalDigits]] is undefined, then
               1. Let maximumFractionDigits be 9𝔽.
               2. Let minimumFractionDigits be +0𝔽.
            3. Else,
               1. Let maximumFractionDigits be durationFormat.[[FractionalDigits]].
               2. Let minimumFractionDigits be durationFormat.[[FractionalDigits]].
            4. Perform ! CreateDataPropertyOrThrow(nfOpts, "maximumFractionDigits", maximumFractionDigits).
            5. Perform ! CreateDataPropertyOrThrow(nfOpts, "minimumFractionDigits", minimumFractionDigits).
            6. Perform ! CreateDataPropertyOrThrow(nfOpts, "roundingMode", "trunc").
            7. Set numericUnitFound to true.
      3. If value is not 0 or display is "always", then
         1. Let numberingSystem be durationFormat.[[NumberingSystem]].
         2. Perform ! CreateDataPropertyOrThrow(nfOpts, "numberingSystem", numberingSystem).
         3. If signDisplayed is true, then
            1. Set signDisplayed to false.
            2. If value is 0 and DurationSign(duration) is -1, then
               1. Set value to negative-zero.
         4. Else,
            1. Perform ! CreateDataPropertyOrThrow(nfOpts, "signDisplay", "never").
         5. Let numberFormatUnit be the NumberFormat Unit value of the current row.
         6. Perform ! CreateDataPropertyOrThrow(nfOpts, "style", "unit").
         7. Perform ! CreateDataPropertyOrThrow(nfOpts, "unit", numberFormatUnit).
         8. Perform ! CreateDataPropertyOrThrow(nfOpts, "unitDisplay", style).
         9. Let nf be ! Construct(%Intl.NumberFormat%, « durationFormat.[[Locale]], nfOpts »).
         10. Let parts be PartitionNumberPattern(nf, value).
         11. Let list be a new empty List.
         12. For each Record { [[Type]], [[Value]] } part of parts, do
         13. Append the Record { [[Type]]: part.[[Type]], [[Value]]: part.[[Value]], [[Unit]]: numberFormatUnit } to list.
         14. Append list to result.
5. Return ListFormatParts(durationFormat, result).

# 15.10 Properties of the Intl.DurationFormat Prototype Object

# 15.10.1 Intl.DurationFormat.prototype.format ( ~~duration~~ durationLike )

When the `format` method is called with an argument ~~duration~~ durationLike, the following steps are taken:

1. Let df be the this value.
2. Perform ? RequireInternalSlot(df, [[InitializedDurationFormat]]).
3. Let ~~record~~ duration be ? ~~ToDurationRecord(duration)~~ToTemporalDuration(durationLike).
4. Let parts be PartitionDurationFormatPattern(df, ~~record~~ duration).
5. Let result be the empty String.
6. For each Record { [[Type]], [[Value]], [[Unit]] } part in parts, do
   1. Set result to the string-concatenation of result and part.[[Value]].
7. Return result.

# 15.10.2 Intl.DurationFormat.prototype.formatToParts ( ~~duration~~ durationLike )

When the `formatToParts` method is called with an argument ~~duration~~ durationLike, the following steps are taken:

1. Let df be the this value.
2. Perform ? RequireInternalSlot(df, [[InitializedDurationFormat]]).
3. Let ~~record~~ duration be ? ~~ToDurationRecord(duration)~~ToTemporalDuration(durationLike).
4. Let parts be PartitionDurationFormatPattern(df, ~~record~~ duration).
5. Let result be ! ArrayCreate(0).
6. Let n be 0.
7. For each Record { [[Type]], [[Value]], [[Unit]] } part in parts, do
   1. Let obj be OrdinaryObjectCreate(%Object.prototype%).
   2. Perform ! CreateDataPropertyOrThrow(obj, "type", part.[[Type]]).
   3. Perform ! CreateDataPropertyOrThrow(obj, "value", part.[[Value]]).
   4. If part.[[Unit]] is not empty, perform ! CreateDataPropertyOrThrow(obj, "unit", part.[[Unit]]).
   5. Perform ! CreateDataPropertyOrThrow(result, ! ToString(n), obj).
   6. Set n to n \+ 1.
8. Return result.

# 15.11 Locale Sensitive Functions of the ECMAScript Language Specification

# 15.11.1 Properties of the Temporal.Duration Prototype Object

# 15.11.1.1 Temporal.Duration.prototype.toLocaleString ( [ locales [ , options ] ] )

This definition supersedes the definition provided in 7.3.24.

This method performs the following steps when called:

1. Let duration be the this value.
2. Perform ? RequireInternalSlot(duration, [[InitializedTemporalDuration]]).
3. Let formatter be ? Construct(%Intl.DurationFormat%, « locales, options »).
4. Let parts be PartitionDurationFormatPattern(formatter, duration).
5. Let result be the empty String.
6. For each Record { [[Type]], [[Value]], [[Unit]] } part in parts, do
   1. Set result to the string-concatenation of result and part.[[Value]].
7. Return result.

# 15.11.2 Properties of the Temporal.Instant Prototype Object

# 15.11.2.1 Temporal.Instant.prototype.toLocaleString ( [ locales [ , options ] ] )

This definition supersedes the definition provided in 8.3.12.

This method performs the following steps when called:

1. Let instant be the this value.
2. Perform ? RequireInternalSlot(instant, [[InitializedTemporalInstant]]).
3. Let dateFormat be ? CreateDateTimeFormat(%Intl.DateTimeFormat%, locales, options, any, all).
4. Return ? FormatDateTime(dateFormat, instant).

# 15.11.3 Properties of the Temporal.PlainDate Prototype Object

# 15.11.3.1 Temporal.PlainDate.prototype.toLocaleString ( [ locales [ , options ] ] )

This definition supersedes the definition provided in 3.3.31.

This method performs the following steps when called:

1. Let plainDate be the this value.
2. Perform ? RequireInternalSlot(plainDate, [[InitializedTemporalDate]]).
3. Let dateFormat be ? CreateDateTimeFormat(%Intl.DateTimeFormat%, locales, options, date, date).
4. Return ? FormatDateTime(dateFormat, plainDate).

# 15.11.4 Properties of the Temporal.PlainDateTime Prototype Object

# 15.11.4.1 Temporal.PlainDateTime.prototype.toLocaleString ( [ locales [ , options ] ] )

This definition supersedes the definition provided in 5.3.35.

This method performs the following steps when called:

1. Let plainDateTime be the this value.
2. Perform ? RequireInternalSlot(plainDateTime, [[InitializedTemporalDateTime]]).
3. Let dateFormat be ? CreateDateTimeFormat(%Intl.DateTimeFormat%, locales, options, any, all).
4. Return ? FormatDateTime(dateFormat, plainDateTime).

# 15.11.5 Properties of the Temporal.PlainMonthDay Prototype Object

# 15.11.5.1 Temporal.PlainMonthDay.prototype.toLocaleString ( [ locales [ , options ] ] )

This definition supersedes the definition provided in 10.3.9.

This method performs the following steps when called:

1. Let plainMonthDay be the this value.
2. Perform ? RequireInternalSlot(plainMonthDay, [[InitializedTemporalMonthDay]]).
3. Let dateFormat be ? CreateDateTimeFormat(%Intl.DateTimeFormat%, locales, options, date, date).
4. Return ? FormatDateTime(dateFormat, plainMonthDay).

# 15.11.6 Properties of the Temporal.PlainTime Prototype Object

# 15.11.6.1 Temporal.PlainTime.prototype.toLocaleString ( [ locales [ , options ] ] )

This definition supersedes the definition provided in 4.3.17.

This method performs the following steps when called:

1. Let plainTime be the this value.
2. Perform ? RequireInternalSlot(plainTime, [[InitializedTemporalTime]]).
3. Let dateFormat be ? CreateDateTimeFormat(%Intl.DateTimeFormat%, locales, options, time, time).
4. Return ? FormatDateTime(dateFormat, plainTime).

# 15.11.7 Properties of the Temporal.PlainYearMonth Prototype Object

# 15.11.7.1 Temporal.PlainYearMonth.prototype.toLocaleString ( [ locales [ , options ] ] )

This definition supersedes the definition provided in 9.3.20.

This method performs the following steps when called:

1. Let plainYearMonth be the this value.
2. Perform ? RequireInternalSlot(plainYearMonth, [[InitializedTemporalYearMonth]]).
3. Let dateFormat be ? CreateDateTimeFormat(%Intl.DateTimeFormat%, locales, options, date, date).
4. Return ? FormatDateTime(dateFormat, plainYearMonth).

# 15.11.8 Properties of the Temporal.ZonedDateTime Prototype Object

# 15.11.8.1 Temporal.ZonedDateTime.prototype.toLocaleString ( [ locales [ , options ] ] )

This definition supersedes the definition provided in 6.3.42.

This method performs the following steps when called:

1. Let zonedDateTime be the this value.
2. Perform ? RequireInternalSlot(zonedDateTime, [[InitializedTemporalZonedDateTime]]).
3. Let dateTimeFormat be ? CreateDateTimeFormat(%Intl.DateTimeFormat%, locales, options, any, all, zonedDateTime.[[TimeZone]]).
4. If zonedDateTime.[[Calendar]] is not "iso8601" and CalendarEquals(zonedDateTime.[[Calendar]], dateTimeFormat.[[Calendar]]) is false, throw a RangeError exception.
5. Let instant be ! CreateTemporalInstant(zonedDateTime.[[EpochNanoseconds]]).
6. Return ? FormatDateTime(dateTimeFormat, instant).

# Copyright & Software License

## Copyright Notice

© 2026 Maggie Pint, Matt Johnson, Brian Terlson, Daniel Ehrenberg, Philipp Dunkel, Sasha Pierson, Ujjwal Sharma, Philip Chimento, Justin Grant

## Software License

All Software contained in this document ("Software") is protected by copyright and is being made available under the "BSD License", included below. This Software may be subject to third party rights (rights from parties other than Ecma International), including patent rights, and no licenses under such third party rights are granted under this license even if the third party concerned is a member of Ecma International. SEE THE ECMA CODE OF CONDUCT IN PATENT MATTERS AVAILABLE AT https://ecma-international.org/memento/codeofconduct.htm FOR INFORMATION REGARDING THE LICENSING OF PATENT CLAIMS THAT ARE REQUIRED TO IMPLEMENT ECMA INTERNATIONAL STANDARDS.

Redistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the following disclaimer in the documentation and/or other materials provided with the distribution.
3. Neither the name of the authors nor Ecma International may be used to endorse or promote products derived from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE ECMA INTERNATIONAL "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL ECMA INTERNATIONAL BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
