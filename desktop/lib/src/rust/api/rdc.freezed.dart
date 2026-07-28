// GENERATED CODE - DO NOT MODIFY BY HAND
// coverage:ignore-file
// ignore_for_file: type=lint
// ignore_for_file: unused_element, deprecated_member_use, deprecated_member_use_from_same_package, use_function_type_syntax_for_parameters, unnecessary_const, avoid_init_to_null, invalid_override_different_default_values_named, prefer_expression_function_bodies, annotate_overrides, invalid_annotation_target, unnecessary_question_mark

part of 'rdc.dart';

// **************************************************************************
// FreezedGenerator
// **************************************************************************

// dart format off
T _$identity<T>(T value) => value;
/// @nodoc
mixin _$SyncPhase {





@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
  return 'SyncPhase()';
}


}

/// @nodoc
class $SyncPhaseCopyWith<$Res>  {
$SyncPhaseCopyWith(SyncPhase _, $Res Function(SyncPhase) __);
}


/// Adds pattern-matching-related methods to [SyncPhase].
extension SyncPhasePatterns on SyncPhase {
/// A variant of `map` that fallback to returning `orElse`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeMap<TResult extends Object?>({TResult Function( SyncPhase_Started value)?  started,TResult Function( SyncPhase_Log value)?  log,TResult Function( SyncPhase_Done value)?  done,TResult Function( SyncPhase_Error value)?  error,required TResult orElse(),}){
final _that = this;
switch (_that) {
case SyncPhase_Started() when started != null:
return started(_that);case SyncPhase_Log() when log != null:
return log(_that);case SyncPhase_Done() when done != null:
return done(_that);case SyncPhase_Error() when error != null:
return error(_that);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// Callbacks receives the raw object, upcasted.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case final Subclass2 value:
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult map<TResult extends Object?>({required TResult Function( SyncPhase_Started value)  started,required TResult Function( SyncPhase_Log value)  log,required TResult Function( SyncPhase_Done value)  done,required TResult Function( SyncPhase_Error value)  error,}){
final _that = this;
switch (_that) {
case SyncPhase_Started():
return started(_that);case SyncPhase_Log():
return log(_that);case SyncPhase_Done():
return done(_that);case SyncPhase_Error():
return error(_that);}
}
/// A variant of `map` that fallback to returning `null`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? mapOrNull<TResult extends Object?>({TResult? Function( SyncPhase_Started value)?  started,TResult? Function( SyncPhase_Log value)?  log,TResult? Function( SyncPhase_Done value)?  done,TResult? Function( SyncPhase_Error value)?  error,}){
final _that = this;
switch (_that) {
case SyncPhase_Started() when started != null:
return started(_that);case SyncPhase_Log() when log != null:
return log(_that);case SyncPhase_Done() when done != null:
return done(_that);case SyncPhase_Error() when error != null:
return error(_that);case _:
  return null;

}
}
/// A variant of `when` that fallback to an `orElse` callback.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeWhen<TResult extends Object?>({TResult Function()?  started,TResult Function( String line)?  log,TResult Function( BigInt fileCount)?  done,TResult Function( String message)?  error,required TResult orElse(),}) {final _that = this;
switch (_that) {
case SyncPhase_Started() when started != null:
return started();case SyncPhase_Log() when log != null:
return log(_that.line);case SyncPhase_Done() when done != null:
return done(_that.fileCount);case SyncPhase_Error() when error != null:
return error(_that.message);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// As opposed to `map`, this offers destructuring.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case Subclass2(:final field2):
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult when<TResult extends Object?>({required TResult Function()  started,required TResult Function( String line)  log,required TResult Function( BigInt fileCount)  done,required TResult Function( String message)  error,}) {final _that = this;
switch (_that) {
case SyncPhase_Started():
return started();case SyncPhase_Log():
return log(_that.line);case SyncPhase_Done():
return done(_that.fileCount);case SyncPhase_Error():
return error(_that.message);}
}
/// A variant of `when` that fallback to returning `null`
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? whenOrNull<TResult extends Object?>({TResult? Function()?  started,TResult? Function( String line)?  log,TResult? Function( BigInt fileCount)?  done,TResult? Function( String message)?  error,}) {final _that = this;
switch (_that) {
case SyncPhase_Started() when started != null:
return started();case SyncPhase_Log() when log != null:
return log(_that.line);case SyncPhase_Done() when done != null:
return done(_that.fileCount);case SyncPhase_Error() when error != null:
return error(_that.message);case _:
  return null;

}
}

}

/// @nodoc


class SyncPhase_Started extends SyncPhase {
  const SyncPhase_Started(): super._();
  






@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Started);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
  return 'SyncPhase.started()';
}


}




/// @nodoc


class SyncPhase_Log extends SyncPhase {
  const SyncPhase_Log({required this.line}): super._();
  

 final  String line;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$SyncPhase_LogCopyWith<SyncPhase_Log> get copyWith => _$SyncPhase_LogCopyWithImpl<SyncPhase_Log>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Log&&(identical(other.line, line) || other.line == line));
}


@override
int get hashCode => Object.hash(runtimeType,line);

@override
String toString() {
  return 'SyncPhase.log(line: $line)';
}


}

/// @nodoc
abstract mixin class $SyncPhase_LogCopyWith<$Res> implements $SyncPhaseCopyWith<$Res> {
  factory $SyncPhase_LogCopyWith(SyncPhase_Log value, $Res Function(SyncPhase_Log) _then) = _$SyncPhase_LogCopyWithImpl;
@useResult
$Res call({
 String line
});




}
/// @nodoc
class _$SyncPhase_LogCopyWithImpl<$Res>
    implements $SyncPhase_LogCopyWith<$Res> {
  _$SyncPhase_LogCopyWithImpl(this._self, this._then);

  final SyncPhase_Log _self;
  final $Res Function(SyncPhase_Log) _then;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? line = null,}) {
  return _then(SyncPhase_Log(
line: null == line ? _self.line : line // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

/// @nodoc


class SyncPhase_Done extends SyncPhase {
  const SyncPhase_Done({required this.fileCount}): super._();
  

 final  BigInt fileCount;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$SyncPhase_DoneCopyWith<SyncPhase_Done> get copyWith => _$SyncPhase_DoneCopyWithImpl<SyncPhase_Done>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Done&&(identical(other.fileCount, fileCount) || other.fileCount == fileCount));
}


@override
int get hashCode => Object.hash(runtimeType,fileCount);

@override
String toString() {
  return 'SyncPhase.done(fileCount: $fileCount)';
}


}

/// @nodoc
abstract mixin class $SyncPhase_DoneCopyWith<$Res> implements $SyncPhaseCopyWith<$Res> {
  factory $SyncPhase_DoneCopyWith(SyncPhase_Done value, $Res Function(SyncPhase_Done) _then) = _$SyncPhase_DoneCopyWithImpl;
@useResult
$Res call({
 BigInt fileCount
});




}
/// @nodoc
class _$SyncPhase_DoneCopyWithImpl<$Res>
    implements $SyncPhase_DoneCopyWith<$Res> {
  _$SyncPhase_DoneCopyWithImpl(this._self, this._then);

  final SyncPhase_Done _self;
  final $Res Function(SyncPhase_Done) _then;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? fileCount = null,}) {
  return _then(SyncPhase_Done(
fileCount: null == fileCount ? _self.fileCount : fileCount // ignore: cast_nullable_to_non_nullable
as BigInt,
  ));
}


}

/// @nodoc


class SyncPhase_Error extends SyncPhase {
  const SyncPhase_Error({required this.message}): super._();
  

 final  String message;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$SyncPhase_ErrorCopyWith<SyncPhase_Error> get copyWith => _$SyncPhase_ErrorCopyWithImpl<SyncPhase_Error>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is SyncPhase_Error&&(identical(other.message, message) || other.message == message));
}


@override
int get hashCode => Object.hash(runtimeType,message);

@override
String toString() {
  return 'SyncPhase.error(message: $message)';
}


}

/// @nodoc
abstract mixin class $SyncPhase_ErrorCopyWith<$Res> implements $SyncPhaseCopyWith<$Res> {
  factory $SyncPhase_ErrorCopyWith(SyncPhase_Error value, $Res Function(SyncPhase_Error) _then) = _$SyncPhase_ErrorCopyWithImpl;
@useResult
$Res call({
 String message
});




}
/// @nodoc
class _$SyncPhase_ErrorCopyWithImpl<$Res>
    implements $SyncPhase_ErrorCopyWith<$Res> {
  _$SyncPhase_ErrorCopyWithImpl(this._self, this._then);

  final SyncPhase_Error _self;
  final $Res Function(SyncPhase_Error) _then;

/// Create a copy of SyncPhase
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? message = null,}) {
  return _then(SyncPhase_Error(
message: null == message ? _self.message : message // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

// dart format on
